# Security

switchboard sits between clients and LLM providers, so it handles API keys, tenant data, and
untrusted model output. Treat it accordingly.

## Reporting

Report vulnerabilities privately through
[GitHub Security Advisories](https://github.com/nguyenbatam/switchboard/security/advisories/new)
rather than opening a public issue.

## Threat model

Beyond the usual gateway concerns, this project takes on the risks specific to LLM systems, tracked
against the [OWASP Top 10 for LLM Applications](https://genai.owasp.org/llm-top-10/). These are design
commitments, specified in [docs/](docs/README.md) and not yet implemented — see Status below.

- **Indirect prompt injection.** Documents retrieved by RAG are untrusted input, and so are SQL rows
  and MCP tool results. The defense does not rely on the model ignoring an embedded instruction,
  because it cannot: once untrusted content enters the context the request is *tainted*, and a
  tainted request loses access to every tool that can reach the network or cause a side effect.
  Privileges are made safe rather than text.
  ([docs/06 §4](docs/06-agent-tools.md#4-prompt-injection-taint-and-capability-downgrade))
- **Excessive agency.** The agent loop executes tools. `sql_query` is parsed to an AST rather than
  regex-filtered, restricted to an allowlist of tables, bounded in rows and time, and — independently
  of all of that — runs as a Postgres role holding no write grant on anything.
- **Data exfiltration through tools.** A tool call is an outbound channel and is treated as one.
  `wallet_balance` takes an address, never a URL, and its egress is restricted to a fixed RPC host
  list: a tool that accepts a URL from a model is an SSRF primitive with extra steps.
- **Provider credentials.** Upstream API keys live in the environment. Tenant API keys are stored in
  Postgres as argon2id hashes, never in plaintext.
- **Cross-tenant leakage.** Every tenant-scoped table filters in the application *and* under Postgres
  row-level security, and nothing — including the semantic cache — is shared across tenants.

Injection resistance is measured, not asserted: the eval suite carries an attack corpus, and the
attack success rate is published next to the accuracy numbers.
([docs/09](docs/09-eval.md))

## Status

Early development. This is not production software. Do not point it at data that matters yet.
