# web-scaffolding
This is a collection of code pieces I have (previously) copy-pasted across my axum projects. Think of it as a highly opinionated web application template.

I do not recommend depending on this as a crate, however feel free to copy-paste parts of the code.

# features
- configuration (cli + json + env)
- database orm + in-memory stores
- reverse proxy integration
- sensible middleware
- asset inlining[^1]
- typed cookies
- user sessions
- htmx integration
- various qol

[^1]: release build is a single binary