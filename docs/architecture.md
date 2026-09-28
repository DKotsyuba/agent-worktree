# Architecture

Candidate profile: in-process + none, stdio MCP, no host adapter.
A single executable registers one read-only identity tool from schemas/tools.json.
No filesystem/business state, installer, background scheduler or network client is implemented.

This deliberately small starter does not encode a multi-crate runtime framework.
Separate domain/application/adapters when real product behavior requires boundaries.
