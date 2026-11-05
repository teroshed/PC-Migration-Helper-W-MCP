import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { GatewayClient, validateDescriptor } from '../scripts/gateway-client.mjs';

const token='a'.repeat(64),instanceId='test-instance-1234567890';
test('descriptor rejects public endpoints, URLs with credentials and malformed identities',()=>{
  const base={schemaVersion:1,endpoint:'http://127.0.0.1:1234/rpc',token,instanceId,pid:1234};
  assert.equal(validateDescriptor(base).endpoint,base.endpoint);
  for(const endpoint of ['https://127.0.0.1:1234/rpc','http://localhost:1234/rpc','http://192.168.1.1:1234/rpc','http://user:secret@127.0.0.1:1234/rpc','http://127.0.0.1:1234/rpc?token=leak'])assert.throws(()=>validateDescriptor({...base,endpoint}));
  assert.throws(()=>validateDescriptor({...base,token:'short'}));
});
test('bridge authenticates and fences identity, rereads descriptor, and never retries a failed mutation',async()=>{
  let calls=0;let reject=false;
  const server=createServer(async(req,res)=>{
    calls++;
    assert.equal(req.headers.authorization,`Bearer ${token}`);
    assert.equal(req.headers['x-migration-instance'],instanceId);
    assert.equal(req.headers.origin,undefined);
    const chunks=[];for await(const c of req)chunks.push(c);
    const body=JSON.parse(Buffer.concat(chunks));
    res.setHeader('Content-Type','application/json');
    res.end(JSON.stringify(reject?{ok:false,error:'revision conflict'}:{ok:true,result:{method:body.method,revision:7}}));
  });
  server.listen(0,'127.0.0.1');await once(server,'listening');
  try{
    const dir=await mkdtemp(join(tmpdir(),'migration-mcp-client-'));
    const file=join(dir,'gateway.json');
    await writeFile(file,JSON.stringify({schemaVersion:1,endpoint:`http://127.0.0.1:${server.address().port}/rpc`,token,instanceId,pid:process.pid}));
    const client=new GatewayClient(file);
    assert.equal((await client.call('get_state')).revision,7);
    await assert.rejects(client.call('mutate',{operationId:'same-edit',expectedInstanceId:'old-instance-1234567890'}),/session changed/);assert.equal(calls,1);
    for (const method of ['audit_control', 'audit_read_notifications']) {
      await assert.rejects(client.call(method, { expectedInstanceId: 'old-instance-1234567890' }), /session changed/);
      await assert.rejects(client.call(method), /session changed/);
      assert.equal(calls, 1, 'A stale checksum/notification write must never be sent');
    }
    reject=true;await assert.rejects(client.call('mutate',{operationId:'same-edit',expectedInstanceId:instanceId}),/revision conflict/);assert.equal(calls,2);
    await writeFile(file,'invalid descriptor');await assert.rejects(client.call('get_state'),/not ready/);assert.equal(calls,2);
  }finally{server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
});
