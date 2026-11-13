# Security policy

Dockwren holds access to the Docker socket, which is equivalent to root on the host, so security
reports are taken seriously.

## Reporting a vulnerability

Please do not open a public issue. Use GitHub's private vulnerability reporting on this repository
("Security" tab, "Report a vulnerability"). Include the version, what you did and what happened.
You will get an answer within a few days; fixes ship as a new release with a note in the changelog.

## Supported versions

Only the latest release receives fixes while the project is pre-1.0.

## Hardening notes

See "Security notes" in the README: keep the default loopback bind, put TLS and an identity layer
in front, and never expose the panel on a public port without HTTPS.
