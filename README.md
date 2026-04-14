# web-scaffolding
This is a collection of code pieces I have copy-pasted across my axum projects.

I do not recommend depending on this as a crate, however feel free to copy-paste parts of the code.

# stack
- framework: axum
- errors: eyre + simple-eyre + tracing
- config: toml
- assets: rust-embed
- session: scc + tower-cookies
- database: sqlite + toasty