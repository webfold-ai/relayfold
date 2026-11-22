# Security policy

relayfold is the data-plane and control-plane code for proxy fleets, so a vulnerability can mean
traffic reaching the wrong upstream, a forged config push being accepted, or a crash from a crafted
request.

## Reporting a vulnerability

Do not open a public issue. Use GitHub's private vulnerability reporting for this repository
(Security tab, "Report a vulnerability"), or email `security@webfold.ai`.

Include the commit you tested, a minimal reproducer, and the impact you believe it has. You should
get an acknowledgement within 3 business days.

## Scope

In scope: every workspace member (`relayfold`, `relayfold-agents`, `relayfold-control`,
`relayfold-convert`, `relayfold-proxy-runtime`, `relayfold-bench`) and the Helm chart under
`relayfold-helm/`. Out of scope: vulnerabilities in third-party servers the benchmark harness is
pointed at.
