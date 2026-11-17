import assert from 'node:assert/strict';
import { ArgentClient, lineTransport, type ArgentEvent } from '../src/index.ts';

const sent: string[] = [];
const client = new ArgentClient(lineTransport((line) => sent.push(line)));

const events: ArgentEvent[] = [];
const prompt = client.prompt('list the crates', (event) => events.push(event));
assert.deepEqual(JSON.parse(sent[0]!), { id: 1, method: 'prompt', params: { text: 'list the crates' } });

client.feed('{"id":1,"type":"event","event":"text","data":{"text":"crates"}}');
client.feed('{"id":2,"type":"event","event":"text","data":{"text":"other"}}');
client.feed('{"id":1,"type":"result","data":{"reason":"complete","tokens":12}}');
assert.deepEqual(await prompt, { reason: 'complete', tokens: 12 });
assert.equal(events.length, 1);
assert.equal(events[0]!.event, 'text');

const models = client.models();
assert.match(sent[1]!, /"method":"models"/);
client.feed('{"id":2,"type":"result","data":{"models":[{"id":"spark-4b"}]}}');
assert.deepEqual(await models, [{ id: 'spark-4b' }]);

client.feed('{"id":99,"type":"result","data":{}}');
console.log('sdk smoke ok');
