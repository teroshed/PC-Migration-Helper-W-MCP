export type ClosePhase = 'idle' | 'saving' | 'stopping' | 'waiting' | 'hiding';
export interface CloseState { phase: ClosePhase; ticket: number | null }
export function assertNewCloseAllowed(state: CloseState, native: Record<string, unknown>): void {
  if (state.phase !== 'idle' || state.ticket !== null || native.pending !== null || native.draining !== false) {
    throw new Error('Exit is already pending. Inspect its ticket and use Retry Exit or Cancel Exit.');
  }
}
export interface CloseServices {
  status: () => Promise<{ enabled: boolean; ready: boolean }>;
  begin: () => Promise<number>;
  flush: () => Promise<void>;
  hide: () => Promise<void>;
  confirm: (ticket: number) => Promise<void>;
  cancel: (ticket: number) => Promise<void>;
}

/** Close owns a ticket before flushing. A stop timeout retains that ticket so
 * jobs cannot restart until an explicit retry or cancellation. */
export class CloseSession {
  private state: CloseState = { phase: 'idle', ticket: null };
  private inFlight: Promise<void> | null = null;
  private services: CloseServices;
  private changed: (state: CloseState) => void;
  constructor(services: CloseServices, changed: (state: CloseState) => void) {
    this.services = services;
    this.changed = changed;
  }
  snapshot(): CloseState { return { ...this.state }; }
  private update(phase: ClosePhase, ticket: number | null): void {
    this.state = { phase, ticket };
    this.changed(this.snapshot());
  }
  request(explicitTicket?: number): Promise<void> {
    if (this.inFlight) return this.inFlight;
    const task = this.run(explicitTicket).finally(() => { if (this.inFlight === task) this.inFlight = null; });
    this.inFlight = task;
    return task;
  }
  async cancel(): Promise<void> {
    if (this.inFlight || this.state.phase !== 'waiting' || this.state.ticket === null) throw new Error('Wait for the current close attempt before canceling Exit.');
    await this.services.cancel(this.state.ticket);
    this.update('idle', null);
  }
  private async run(explicitTicket?: number): Promise<void> {
    let ticket = explicitTicket ?? this.state.ticket;
    let confirming = ticket !== null && this.state.phase === 'waiting';
    try {
      const status = await this.services.status();
      if (ticket === null && status.enabled && status.ready) {
        this.update('saving', null);
        await this.services.flush();
        this.update('hiding', null);
        await this.services.hide();
        this.update('idle', null);
        return;
      }
      ticket = ticket ?? await this.services.begin();
      this.update('saving', ticket);
      await this.services.flush();
      confirming = true;
      this.update('stopping', ticket);
      await this.services.confirm(ticket);
    } catch (cause) {
      if (ticket !== null && !confirming) {
        try { await this.services.cancel(ticket); this.update('idle', null); }
        catch { this.update('waiting', ticket); }
      } else this.update(confirming ? 'waiting' : 'idle', ticket);
      throw cause;
    }
  }
}
