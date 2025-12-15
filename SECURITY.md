# Security Policy

Greentic is designed as a security-first platform for building, distributing, and running trusted digital workers, components, and packs across multi-tenant environments. We take security vulnerabilities seriously and appreciate responsible disclosure from the community.

---

## Supported Versions

Security updates are provided only for versions that are actively maintained.

| Version Range | Supported |
|--------------|-----------|
| `0.4.x` (current stable) | ✅ Yes |
| `0.3.x` | ❌ No |
| `< 0.3.x` | ❌ No |

> **Note**
> - Only the latest minor release within a supported version line receives security fixes.
> - Users are strongly encouraged to upgrade to the latest supported version as soon as possible.

---

## Reporting a Vulnerability

If you believe you have found a security vulnerability in Greentic or any of its official repositories, **please do not open a public GitHub issue**.

### How to Report

Send a detailed report to:

**security@greentic.ai**

Please include as much of the following information as possible:

- A clear description of the vulnerability
- Affected repository/repositories and versions
- Steps to reproduce (proof-of-concept code is welcome)
- Potential impact and realistic attack scenarios
- Any suggested mitigations or fixes (if known)

Reports may be submitted anonymously if preferred.

---

## What to Expect After Reporting

- **Acknowledgement**: You will receive an acknowledgement within **3 business days**.
- **Assessment**: The Greentic security team will assess severity, scope, and impact.
- **Status Updates**: Periodic updates will be provided (typically every **7–14 days**) until resolution.
- **Fix & Disclosure**:
  - If accepted, a fix will be developed and released for supported versions.
  - Coordinated disclosure will be agreed upon prior to any public announcement.
- **Declined Reports**: If a report is declined (for example, expected behavior or low impact), an explanation will be provided.

---

## Scope

This security policy applies to:

- All **official Greentic repositories**
- Core Rust crates, CLIs, and runtime services
- WASM components, host bindings, and execution environments
- Pack, flow, and plugin infrastructure
- Secrets management, configuration, and state handling
- Supply-chain tooling (build, signing, provenance, metadata, OCI publishing)

Third-party integrations, downstream forks, or unofficial plugins are out of scope unless explicitly stated.

---

## Secure Development Practices

Greentic follows modern secure-by-design principles, including:

- Least-privilege execution via WASM sandboxing and explicit capabilities
- Explicit secrets handling with no implicit environment leakage
- Supply-chain hardening through hashing, provenance, and OCI-based distribution
- Strongly typed interfaces (WIT contracts and schema validation)
- Defense-in-depth through validation, redaction, and isolation
- Auditability via structured telemetry and execution traces

Security-relevant changes receive heightened review.

---

## Responsible Disclosure & Safe Harbor

We support responsible security research.

If you:

- Make a good-faith effort to avoid privacy violations or data destruction
- Do not exploit vulnerabilities beyond minimal proof-of-concept
- Do not publicly disclose issues before coordinated resolution

Then we consider your research authorized and will not pursue legal action against you.

---

## Acknowledgements

We are happy to acknowledge security researchers who help improve Greentic, unless anonymity is requested.

---

Thank you for helping keep Greentic and its users secure.
