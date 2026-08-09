//! `ikigai-web` — browse an ikigai kernel from a web browser.
//!
//! A small standalone HTTP server that makes kernel resources browsable:
//! `GET http://127.0.0.1:{port}/{uri}` resolves `{uri}` through an embedded
//! kernel composed from the machine's normal config-home mounts. Loopback
//! only; the trust model is the local owner. The face lands in the first PR;
//! this commit is the repo skeleton (CI from day one).
