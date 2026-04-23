# web-scaffolding
A web application template. Batteries included, kitchen sink optional.

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

# getting started
To start a server, implement the WebServer trait and call run!(...)

```rust
use eyre::Result;
use web_scaffolding::{WebServer, Router, assets, run};

struct App;

impl WebServer for App {
    assets!("assets_dir/");

    async fn init(self) -> Result<Router<Self>> {
        let router = Router::new()
            .route("/", ...)

        Ok(router)
    }
}

run!(App);
```

Now you have a working web server with tracing and static asset handling. Check this crate's features to learn what else is available.

For now, web-scaffolding is intentionally not published on crates.io. The crate uses cargo features extensively, and those are notoriously hard to manage in a semver-compatible manner. Depend on a pinned git commit instead.

You can also just copy-paste parts of the code! The `src/utils` module is especially designed to be modular enough for this.