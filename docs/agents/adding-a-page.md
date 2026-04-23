# Agent playbook — Adding a page

To add a new HTML page:

1. **Template.** Create `templates/<name>.html` extending `templates/base.html`. Keep it
   compile-checked by using `{% extends %}` and referencing fields on a typed struct.
2. **Handler.** Add a handler function in `src/http/pages.rs` that constructs the context
   struct, queries any needed data via `sqlx`, and renders the template.
3. **Route.** Register the route in `src/http/routes.rs` using `get(page_handler)`.
4. **Links.** Link to the new page from wherever is appropriate (header nav, card actions).
   Percent-encode any text placed into URL paths or queries via `util::url`.
5. **Styles.** Add any page-specific CSS to `static/app.css`. Reuse component classes.
6. **JS.** If interactivity is needed, add a vanilla JS module under `static/`. Do not add a
   bundler.
7. **Docs.** Update `docs/design/10-ui.md` and, if relevant, `docs/design/09-http-api.md`.

Before committing: `just fmt && just lint && just check && just test`.
