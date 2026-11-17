# Hosted / remote mode (MVP)

`argent serve` turns one workspace into an HTTP endpoint. It is the basis for
both hosted runs and the small mobile web client.

```powershell
$token = -join ((48..57) + (97..122) | Get-Random -Count 32 | ForEach-Object {[char]$_})
argent serve --port 7777 --token $token
```

- Open `http://host:7777/` on a phone or another machine: the page is served
  by argent itself (`web/index.html`), remembers the pairing token in
  `sessionStorage`, streams events from `/v1/events`, and posts prompts to
  `/v1/prompt`.
- `EventSource` cannot send an `Authorization` header, so the SSE endpoint
  accepts the same token as `?token=…`. Fetch endpoints use
  `Authorization: Bearer …`. `GET /v1/health` stays open for probes.
- Terminate TLS in a reverse proxy; never expose the plain port to the
  internet. A minimal Caddyfile:

```text
argent.example.com {
  reverse_proxy 127.0.0.1:7777
}
```

Each server process owns one agent/workspace. Run one process per tenant with
its own token and cwd; a supervisor (systemd/Windows Service) owns restarts.
A multi-tenant scheduler with per-tenant diff storage is not implemented yet.

Security notes: the token grants command execution in the workspace. Treat it
like an SSH key, rotate it by restarting the server, and keep the web page on
trusted devices. Board state (`argent board`) and sessions are per-workspace.
