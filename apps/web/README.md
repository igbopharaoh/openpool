# OpenPool web application

This is the Rust/Dioxus UI binary. Its SSR shell is mounted as the Axum fallback while `/v1`,
health, webhook, and static-asset routes retain precedence. Node and TypeScript are not runtime
dependencies of OpenPool.
