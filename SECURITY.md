# Security

switchboard sits between clients and LLM providers, so it handles API keys, tenant data, and
untrusted model output. Treat it accordingly.

## Reporting

Report vulnerabilities privately through
[GitHub Security Advisories](https://github.com/nguyenbatam/switchboard/security/advisories/new)
rather than opening a public issue.

## Threat model

Beyond the usual gateway concerns, this project takes on the risks specific to LLM systems, tracked
against the [OWASP Top 10 for LLM Applications](https://genai.owasp.org/llm-top-10/):

- **Indirect prompt injection.** Documents retrieved by RAG are untrusted input. Text stored in the
  database can attempt to redirect the model.
- **Excessive agency.** The agent loop executes tools. The `sql_query` tool is read-only and
  restricted to an allowlist of tables.
- **Data exfiltration through tools.** A tool call is an outbound channel and is treated as one.
- **Provider credentials.** Upstream API keys live in the environment. Tenant API keys are stored in
  Postgres as hashes, never in plaintext.

## Status

Early development. This is not production software. Do not point it at data that matters yet.
