# CSI protocol

`csi.proto` is vendored unchanged from the Apache-2.0 licensed
[CSI specification v1.12.0](https://raw.githubusercontent.com/container-storage-interface/spec/v1.12.0/csi.proto).

SHA-256: `5b81236a3809f3ff0b877ff9b82215d0b74a8e291d7f3ec6ee1a44f537c0f86a`.

Bindings are generated locally by `build.rs` with a vendored protoc; building
does not fetch the schema or require a system protoc installation.
