# Security boundary

The starter exposes one read-only identity tool. It has no filesystem mutation,
external SaaS access, daemon, or credential handling. A future product must describe
its actual authorized roots/tenants, subjects and side effects before implementing
mutations. Tool annotations and caller-supplied actor labels are not authentication.

Do not publish secrets or raw user content in logs, snapshots or evidence. Report
security concerns privately to the repository owner; establish a real reporting
channel before public distribution.
