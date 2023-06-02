# async-lsp

[![crates.io](https://img.shields.io/crates/v/async-lsp)](https://crates.io/crates/async-lsp)
[![docs.rs](https://img.shields.io/docsrs/async-lsp)][docs]
[![CI Status](https://github.com/oxalica/async-lsp/actions/workflows/ci.yaml/badge.svg)](https://github.com/oxalica/async-lsp/actions/workflows/ci.yaml)

Asynchronous [Language Server Protocol (LSP)][lsp] framework based on [tower].

[docs]: https://docs.rs/async-lsp
[lsp]: https://microsoft.github.io/language-server-protocol/overviews/lsp/overview/
[tower]: https://github.com/tower-rs/tower

## Overview

This crate is centered at `trait LspService` which mainly consists of a tower
`Service` of LSP requests and a handler of LSP notifications.

As protocol defines, requests can be processed concurrently (asynchronously),
while notifications must be processed in order (synchronously), changing states
and affecting semantics of later requests and/or notifications.

Request handling is designed in a decoupled manner with
[tower-layer](https://crates.io/crates/tower-layer), so you can chain multiple
request processing layer (aka. middleware) to build complex a service.

Despite the name of `LspService`, it can be used to build both Language Server
and Language Client. They are logically symmetric and both using duplex
channels. The only difference is the kind of requests and notifications they
support.

## Usage

See [examples](./examples).

## Similar projects

### [tower-lsp](https://crates.io/crates/tower-lsp)

async-lsp is heavily inspired by tower-lsp, we are both built on tower but have
major design differences.

1.  tower-lsp is less flexible and hard to use with tower ecosystem. It doesn't
    support custom tower `Layer` since the `Service` interface is builtin. Both
    server lifecycle handling and concurrency logic is built-in and is hard to
    opt-opt or customize.

    async-lsp uses tower `Layer` to implement server lifecycle, concurrency,
    tracing and more. Users can select and compose layers, or creating custom
    ones.

1.  tower-lsp handles notifications asynchronously, which is semantically
    incorrect and introduces
    [out-of-order issues](https://github.com/ebkalderon/tower-lsp/issues/284).

    async-lsp executes notification handlers synchronously, and allows it to
    control main loop when, it needs to exit or something goes wrong.

1.  tower-lsp's `trait LanguageServer` accepts immutable state `&self` for
    concurrency. Thus state changing notifications like
    `textDocument/didChange` always requires asynchronous locks, regarding that
    the underlying communication channel is synchronous anyway.

    async-lsp accepts `&mut self` for requests and notifications, and the
    former returns a `Future` without borrowing `self`. Requests borrows only
    immutable states and can be run concurrently, while still being able to
    mutate state (like snapshotting) during preparation.

1.  tower-lsp provides some higher level abstractions over LSP specification to
    make it more ergonomic, like generic `Client::show_message`, simplified
    `LanguageServer::shutdown`, or planned
    [`Progress`-API](https://github.com/ebkalderon/tower-lsp/issues/380).

    While this is not a goal of async-lsp. By default we doesn't do more than
    serialization, deserialization and request/response `id` handling.
    Parameters and interface follows the
    [`lsp-types`](https://crates.io/crates/lsp-types)' `Request` and
    `Notification` traits. But you are still free to implement your custom
    `Request`s for extension, or custom middlewares for higher level API.

1.  tower-lsp is specialized for building Language Servers.

    async-lsp can be used for both Language Servers and Clients.

### [lsp-server](https://crates.io/crates/lsp-server)

lsp-server is a simple and synchronous framework for only Language Server. You
need spawning tasks and managing ongoing requests/responses manually.

## License

async-lsp is distributed under the terms of either the MIT or the Apache 2.0
license, at your option. See [LICENSE-MIT](./LICENSE-MIT) and
[LICENSE-APACHE](./LICENSE-APACHE) for details.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, shall be dual licensed as above, without any
additional terms or conditions.
