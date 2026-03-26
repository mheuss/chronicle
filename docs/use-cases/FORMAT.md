# Use-Case Catalog Format Specification

This document defines how to write and maintain use-case catalog entries. It is
the single source of truth for entry format. When `docs/use-cases/FORMAT.md`
exists in a project, it should match this specification.

## Directory Structure

```
docs/use-cases/
├── INDEX.md          # Domain registry
├── FORMAT.md         # Format specification (copy of this file)
├── authentication.md # Per-domain file
├── payments.md
└── shared.md
```

## INDEX.md

```markdown
# Use-Case Domains

See [FORMAT.md](FORMAT.md) for how to document use-cases.

| Domain | Description | Code Location |
|--------|-------------|---------------|
| authentication | Auth flows, session management | `src/auth/` |
| payments | Payment processing, refunds | `src/payments/` |
| shared | Cross-cutting utilities | `src/helpers/` |
```

## Entry Format

```markdown
## [Use-Case Name]

**Problem:** What problem this solves. One or two sentences.

**Problem indicators:**
- "need to handle concurrent cache invalidation"
- "how do I prevent duplicate event processing"
- "cross-service retry with backoff"

**Location:** `src/path/file.ext:ClassName.methodName`

**Notes:** Implementation constraints, design trade-offs, why alternatives
were rejected. See ADR-007 for the architectural decision behind this approach.
```

### Field Rules

**Problem** — What the code solves, not how it works. One or two sentences.

**Problem indicators** — 3-8 word phrases that a developer or agent would
think or say when facing the same problem. These are search terms, not
descriptions. Write them as natural language a person would type or say:
- Good: "need to retry failed HTTP requests with backoff"
- Bad: "RetryableHttpClient implementation with exponential backoff strategy"

**Location** — File path plus symbol name. The path makes it navigable; the
symbol survives minor file reorganizations.
- Good: `src/helpers/http.ts:RetryableClient.send`
- Bad: `src/helpers/http.ts` (too vague)
- Bad: `RetryableClient` (no path)

**Notes** — Constraints and trade-offs, not what the code does. If someone
could understand it by reading the code, it doesn't belong in Notes. ADR
references go here when a use-case relates to an architectural decision.

## Cross-Domain References

When a use-case spans multiple domains, define it in the primary domain and
reference it from related domains:

```markdown
## [Use-Case Name]

**See:** other-domain.md#use-case-name

**Problem indicators:**
- "phrase for searchability"
```

This keeps the full entry in one place while remaining searchable from related
domains.

## When to Document

Document solutions that are:
- **Non-obvious** — someone would waste time figuring this out
- **Reusable** — applies to more than one feature
- **Subtle** — easy to get wrong or miss
- **Cross-cutting** — spans multiple modules or layers

Do NOT document:
- Trivial CRUD operations
- One-off scripts or migrations
- Standard library usage
- Test utilities (unless they solve a non-obvious testing problem)

## Domain Guidelines

- 3-8 use-cases per domain file. Split if a domain exceeds 10 entries.
- Domain names are lowercase, descriptive nouns (authentication, payments,
  shared — not auth-stuff or pay).
- Every domain has a row in INDEX.md with a description and primary code
  location.

## Maintenance

- **Location fields must resolve** — if the code moved, update the location
- **Problem indicators must be genuine search terms** — review periodically
- **Stale entries are worse than missing entries** — delete entries for removed
  code rather than leaving them
