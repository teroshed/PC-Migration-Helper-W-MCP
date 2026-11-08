import { useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { record } from './model';

export type UiCommand = Record<string, unknown> & { action: string };
export type UiRequest = Record<string, unknown> & { command: UiCommand };
export interface UiParticipant {
  read: () => Record<string, unknown>;
  execute: (command: UiCommand, request: UiRequest) => Promise<unknown>;
}
interface UiBridgeParticipant extends UiParticipant { readForReport: () => Promise<Record<string, unknown>> }
export type UiControlRef = { current: UiParticipant | null };
let registrationLane: Promise<void> = Promise.resolve();

/** One serial shell channel. Reads only report state; they never flush drafts. */
export function useUiBridge(instanceId: string | undefined, participant: UiBridgeParticipant): void {
  const registration = useRef(crypto.randomUUID());
  const latest = useRef(participant);
  latest.current = participant;
  useEffect(() => {
    if (!instanceId) return;
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let uiSessionId: string | null = null;
    let acknowledgements: Record<string, unknown>[] = [];
    const completed = new Set<string>();
    async function tick(): Promise<void> {
      try {
        const sent = acknowledgements;
        await invoke('report_ui_state', { report: {
          expectedInstanceId: instanceId, uiSessionId,
          state: await latest.current.readForReport(), acknowledgements: sent,
        } });
        acknowledgements = acknowledgements.filter(value => !sent.includes(value));
        const response = record(await invoke('poll_ui_commands', {
          expectedInstanceId: instanceId, uiSessionId,
        }));
        if (!active) return;
        const commands = Array.isArray(response.commands) ? response.commands : [];
        for (const input of commands) {
          const delivery = record(input);
          const operationId = delivery.operationId;
          if (typeof operationId !== 'string' || completed.has(operationId)) continue;
          // Claim before awaiting effects: repeated delivery can never execute twice.
          completed.add(operationId);
          const request = record(delivery.request);
          const command = record(request.command);
          try {
            if (typeof command.action !== 'string') throw new Error('Invalid UI command');
            const result = await latest.current.execute(command as UiCommand, { ...request, command } as UiRequest);
            acknowledgements.push({ operationId, status: 'applied', ...(result === undefined ? {} : { result }) });
          } catch (cause) {
            acknowledgements.push({ operationId, status: 'error', error: String(cause) });
          }
        }
      } catch {
        // A disconnected shell retains drafts and acknowledgements. No commands are
        // replayed; the native channel reports stale/error until it reconnects.
      } finally { if (active) timer = setTimeout(() => { void tick(); }, 250); }
    }
    async function register(): Promise<void> {
      let previous: string | null = null;
      while (active) {
        try {
          const result = record(await invoke('begin_ui_session', { expectedInstanceId: instanceId,
            registrationId: registration.current, previousUiSessionId: previous }));
          if (!active) return;
          if (result.registered === true && typeof result.uiSessionId === 'string') {
            uiSessionId = result.uiSessionId; void tick(); return;
          }
          previous = typeof result.currentUiSessionId === 'string' ? result.currentUiSessionId || null : null;
        } catch { await new Promise(resolve => setTimeout(resolve, 250)); }
      }
    }
    const initial = registrationLane.then(async () => { if (active) await register(); });
    registrationLane = initial.catch(() => {});
    return () => { active = false; clearTimeout(timer); };
  }, [instanceId]);
}
