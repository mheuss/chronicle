# Security

applies: all code changes

## Core Rules

applies: all

### Injection Prevention
- [ ] Use parameterized queries — never concatenate user input into SQL
- [ ] Sanitize input reaching CLI commands, file paths, or template engines
- [ ] Validate and escape output to prevent XSS in rendered contexts

### Authentication
- [ ] Never store passwords in plaintext — use bcrypt, argon2, or scrypt
- [ ] Enforce minimum password complexity where applicable
- [ ] Implement account lockout or rate limiting after failed attempts

### Access Control
- [ ] Deny by default — require explicit grants
- [ ] Validate authorization on every state-changing operation
- [ ] Apply least privilege — request only permissions needed

### Cryptographic Safety
- [ ] No hardcoded secrets, keys, tokens, or passwords — use env vars or secret managers
- [ ] No homebrew cryptography — use established libraries
- [ ] Enforce HTTPS/TLS for all sensitive data in transit

### Data Protection
- [ ] Classify data by sensitivity before storing or transmitting
- [ ] Encrypt sensitive data at rest
- [ ] Minimize data collection — don't store what you don't need

### Security Misconfiguration
- [ ] Never expose stack traces or internal paths to end users
- [ ] No default credentials in any environment
- [ ] No overly permissive CORS — whitelist specific origins
- [ ] Keep dependencies updated — check for known vulnerabilities

### Logging & Monitoring
- [ ] Log authentication events (successes and failures)
- [ ] Never log sensitive data (passwords, tokens, PII)
- [ ] Include enough context for incident investigation
