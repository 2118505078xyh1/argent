# @argent/cli-client

A small typed client for `argent --rpc`. No dependencies; Node 22+ with
`--experimental-strip-types` (or compile with `tsc`) is enough.

```ts
import { ArgentClient } from './src/index.ts';

const argent = ArgentClient.spawn('argent', ['--rpc', '-m', 'spark-4b']);
const result = await argent.prompt('list the crates', (event) => {
  if (event.event === 'text') process.stdout.write(String(event.data?.text ?? ''));
});
console.log(result.reason);
argent.abort();
```

Methods: `prompt`, `steer`, `models`, `compact`, `new_session`, `set_model`.
Use `client.request(name, params, onEvent)` for any other RPC method.

Run `npm test` for the NDJSON correlation smoke test and `npm run typecheck`
for strict type checking.
