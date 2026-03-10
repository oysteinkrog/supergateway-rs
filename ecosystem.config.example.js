// Example pm2 ecosystem configuration for supergateway-rs.
//
// Usage:
//   pm2 start ecosystem.config.example.js
//   pm2 reload ecosystem.config.example.js
//   pm2 list
//
// See docs/migration.md for full migration guide from TypeScript.

module.exports = {
  apps: [
    // stdio->SSE: Expose an MCP stdio server over SSE transport.
    {
      name: 'mcp-fetch-sse',
      script: './target/release/supergateway-rs',
      args: '--stdio "uvx mcp-server-fetch" --port 3000 --healthEndpoint /healthz --logLevel info',
      instances: 1,
      autorestart: true,
      watch: false,
      max_memory_restart: '50M',
      env: {
        RUST_BACKTRACE: '1',
      },
    },

    // stdio->WebSocket: Expose an MCP stdio server over WebSocket.
    {
      name: 'mcp-fetch-ws',
      script: './target/release/supergateway-rs',
      args: '--stdio "uvx mcp-server-fetch" --outputTransport ws --port 3001 --healthEndpoint /healthz',
      instances: 1,
      autorestart: true,
      watch: false,
      max_memory_restart: '50M',
    },

    // stdio->Streamable HTTP (stateful): Per-session child processes.
    {
      name: 'mcp-fetch-http-stateful',
      script: './target/release/supergateway-rs',
      args: '--stdio "uvx mcp-server-fetch" --outputTransport streamableHttp --stateful --port 3002 --sessionTimeout 300000 --healthEndpoint /healthz',
      instances: 1,
      autorestart: true,
      watch: false,
      max_memory_restart: '100M',
    },

    // stdio->Streamable HTTP (stateless): Per-request child, auto-init.
    {
      name: 'mcp-fetch-http-stateless',
      script: './target/release/supergateway-rs',
      args: '--stdio "uvx mcp-server-fetch" --outputTransport streamableHttp --port 3003 --healthEndpoint /healthz',
      instances: 1,
      autorestart: true,
      watch: false,
      max_memory_restart: '50M',
    },

    // SSE->stdio client: Bridge a remote SSE server to local stdio.
    {
      name: 'mcp-remote-sse',
      script: './target/release/supergateway-rs',
      args: '--sse http://remote-mcp-server:8080/sse',
      instances: 1,
      autorestart: true,
      watch: false,
      max_memory_restart: '50M',
    },

    // With CORS, custom headers, and OAuth2 bearer token.
    {
      name: 'mcp-fetch-cors',
      script: './target/release/supergateway-rs',
      args: [
        '--stdio "uvx mcp-server-fetch"',
        '--port 3004',
        '--cors http://localhost:3000 http://localhost:5173',
        '--header "X-Request-Source: supergateway"',
        '--oauth2Bearer my-secret-token',
        '--healthEndpoint /healthz',
      ].join(' '),
      instances: 1,
      autorestart: true,
      watch: false,
      max_memory_restart: '50M',
    },
  ],
};
