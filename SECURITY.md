# Security policy

## Reporting a vulnerability

Please report security issues privately, through GitHub's private
vulnerability reporting:
[Report a vulnerability](https://github.com/ExaDev/pg_warren/security/advisories/new)
(the repository's **Security** tab, then **Report a vulnerability**).
Don't open a public issue or pull request for one.

Say what you found, the PostgreSQL major version, the pg_warren version
(`SELECT warren.version()`), and how to reproduce it, if you can.

We aim to acknowledge a report within three working days and to tell you
what we plan to do within ten. We fix it in private, publish a GitHub
security advisory with the release that fixes it, and credit you unless
you'd rather we didn't.

## Supported versions

Until 1.0, fixes go into the latest release only: upgrade with
`ALTER EXTENSION pg_warren UPDATE` in every database that has it (README,
"Install").

## What counts

Anything that lets a role do more than DESIGN.md §7 says it may: read or
change a graph it has no access to, run SQL as another role, gain a
privilege, or crash or corrupt the server. What pg_warren installs, who
can do what, and the known limits are in
[docs/security.md](docs/security.md). The behaviours that page lists as
by design are not vulnerabilities.
