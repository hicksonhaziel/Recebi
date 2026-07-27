# Security policy

Recebi is being built so financial truth is deterministic and outside the LLM.
Phase 1 has no signing or transaction-submission capability.

Do not report secrets in an issue. In particular, never include a private key,
seed phrase, RPC credential, personal data, or a production configuration.
Report a suspected vulnerability privately to the project maintainer before
opening a public issue.

Security invariants for this phase:

- The only MCP tool is `recebi_health`, which accepts no arguments.
- Trusted endpoint and identity values come only from local configuration.
- MCP output omits paths, endpoints, credentials, and key material.
- The core crate has no network, filesystem, MCP, wallet, signer, or LLM
  dependency.
