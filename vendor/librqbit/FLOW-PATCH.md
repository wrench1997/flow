# Flow telemetry extension

Source: crates.io librqbit 9.0.1, upstream commit
`a499d2f243d124e144aef137afe7cb304a6e3f36`, `crates/librqbit`.
Upstream: https://github.com/ikatson/rqbit. License: Apache-2.0 (LICENSE).

Flow adds read-only selected missing-piece coverage from current live peers,
an optional excluded transport address, and choke / pending-request telemetry.
Coverage uses announced bitfields, not proof of delivery or global availability.
The global missing-piece mask is copied before taking peer locks to preserve
upstream lock ordering. Choke telemetry mirrors received protocol messages;
it does not change scheduling or banning policy.

The disabled upstream webui frontend is not included. Runtime downloads and
generated build artifacts are not part of this source copy.
