# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in OMPChamber, please report it responsibly.

**GitHub:** use [private security advisories](https://github.com/Reamd7/ompchamber/security/advisories/new) on this repository.

Please include:
- Description of the vulnerability
- Steps to reproduce
- Affected version(s)
- Potential impact

I'll acknowledge receipt within 48 hours and aim to provide a fix or mitigation as quickly as possible.

**Please do not open public GitHub issues for security vulnerabilities.**

## Scope

OpenChamber handles sensitive context including:
- UI authentication (password-protected sessions, JWT tokens)
- Cloudflare tunnel access (remote connectivity)
- Terminal access (PTY sessions)
- Git credentials and SSH keys
- File system operations

Security reports related to any of these areas are especially appreciated.

## Supported Versions

Security fixes are applied to the latest release. There is no LTS or backport policy at this time.
