//! Route extraction against fixture `routes/*.php` files.
//!
//! The fixtures are real PHP rather than inline snippets because the shapes that break an
//! extractor — a chain split across lines, a group nested in a group — are shapes you get
//! wrong when you write the test input as a one-liner.

use elle_laravel::{HttpMethod, Resolved, Route, RouteAction, extract_routes};

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn routes(name: &str) -> Vec<Route> {
    extract_routes(&fixture(name)).routes
}

/// Looks a route up by its resolved URI. Panics rather than returning an Option: every
/// call site below has already asserted the route should exist.
fn by_uri<'a>(routes: &'a [Route], uri: &str) -> &'a Route {
    routes
        .iter()
        .find(|r| r.uri == Resolved::Known(uri.to_string()))
        .unwrap_or_else(|| panic!("no route for {uri}; found {:?}", uris(routes)))
}

fn uris(routes: &[Route]) -> Vec<&str> {
    routes.iter().filter_map(|r| r.uri.known().map(String::as_str)).collect()
}

fn controller(route: &Route) -> (&str, Option<&str>) {
    match route.action.known() {
        Some(RouteAction::Controller { class, method }) => (class, method.as_deref()),
        other => panic!("expected a controller action, got {other:?}"),
    }
}

fn middleware(route: &Route) -> Vec<&str> {
    route.middleware.iter().filter_map(|m| m.known().map(String::as_str)).collect()
}

fn name(route: &Route) -> Option<&str> {
    route.name.as_ref().and_then(|n| n.known()).map(String::as_str)
}

// ---------------------------------------------------------------------------
// The test that matters most: what we cannot resolve must come back unknown.
// ---------------------------------------------------------------------------

/// RISKS.md #4, as an executable assertion. Every registration in `dynamic.php` has at
/// least one part a static reader cannot determine, and that part must come back
/// `Unknown` — never a plausible-looking guess, never an empty string.
///
/// Honesty is per *field*, not per route: `Route::get('/resolved', [$controller, 'handle'])`
/// has a perfectly knowable URI and an unknowable action, and reporting the URI while
/// admitting the action is exactly right. What would be wrong is claiming the action.
///
/// This is the test to read first if the extractor is ever changed: a diff that makes it
/// fail has traded honesty for coverage, which is the trade the issue forbids.
#[test]
fn every_dynamic_registration_admits_what_it_could_not_resolve() {
    let extraction = extract_routes(&fixture("dynamic.php"));

    for route in &extraction.routes {
        let unresolved_parts = route.uri.unresolved_source().is_some()
            || route.action.unresolved_source().is_some()
            || route.name.as_ref().is_some_and(|n| !n.is_known())
            || route.middleware.iter().any(|m| !m.is_known());

        assert!(
            unresolved_parts,
            "line {} was reported as fully known, but nothing in dynamic.php is: {route:?}",
            route.line
        );
    }

    // And no `Unknown` may be an empty placeholder: it must carry the source that beat it,
    // otherwise the UI cannot explain the gap and this degenerates into the empty-string
    // sentinel the types exist to prevent.
    for route in &extraction.routes {
        for source in
            [route.uri.unresolved_source(), route.action.unresolved_source()].into_iter().flatten()
        {
            assert!(!source.is_empty(), "an Unknown must name the expression it failed on");
        }
    }
}

#[test]
fn a_runtime_controller_is_unknown_rather_than_a_guessed_method() {
    let routes = routes("dynamic.php");
    let route = routes.iter().find(|r| r.line == 13).expect("the [$controller, 'handle'] route");

    // The tempting wrong answer is `Controller { class: "?", method: "handle" }` — the
    // method really is in the source. Reporting it implies a controller we cannot name.
    assert!(!route.action.is_known(), "a runtime controller must not resolve: {:?}", route.action);
    assert_eq!(route.action.unresolved_source(), Some("[$controller, 'handle']"));
}

#[test]
fn interpolated_and_concatenated_uris_are_unknown_not_literal() {
    let routes = routes("dynamic.php");

    // The failure this pins: emitting the source text as if it were the URI, so the user
    // sees a route at `/user/{$id}/profile` that does not exist.
    for route in &routes {
        if let Resolved::Unknown(source) = &route.uri {
            assert!(!source.is_empty(), "an unknown URI must say what defeated it");
        }
    }

    let concat = routes.iter().find(|r| r.line == 19).expect("the concatenated-URI route");
    assert_eq!(concat.uri.unresolved_source(), Some("'/tenant/' . $tenant . '/settings'"));

    let interpolated = routes.iter().find(|r| r.line == 22).expect("the interpolated-URI route");
    assert_eq!(interpolated.uri.unresolved_source(), Some("\"/user/{$id}/profile\""));
}

#[test]
fn a_dynamic_group_prefix_poisons_the_uris_inside_it() {
    let routes = routes("dynamic.php");
    // `/inside` is a literal, but it hangs off a prefix we cannot read, so the full URI is
    // not knowable. Reporting `/inside` would point at a path that 404s.
    let inside = routes.iter().find(|r| r.line == 32).expect("the route inside the dynamic group");
    assert!(!inside.uri.is_known(), "expected unknown, got {:?}", inside.uri);
    assert_eq!(inside.uri.unresolved_source(), Some("$prefix"));
}

#[test]
fn a_dynamic_route_name_is_unknown_but_still_recorded_as_present() {
    let routes = routes("dynamic.php");
    let route = routes.iter().find(|r| r.line == 25).expect("the ->name($routeName) route");

    // `None` would mean "this route has no name", which is false and would hide it from a
    // named-route listing for the wrong reason.
    let name = route.name.as_ref().expect("the route does call ->name(), so it has one");
    assert_eq!(name.unresolved_source(), Some("$routeName"));
}

#[test]
fn a_dynamic_middleware_entry_does_not_discard_the_static_ones() {
    let routes = routes("dynamic.php");
    let route = routes.iter().find(|r| r.line == 28).expect("the mixed-middleware route");

    assert_eq!(middleware(route), vec!["auth"], "the readable middleware must survive");
    assert!(
        route.middleware.iter().any(|m| m.unresolved_source() == Some("$extraMiddleware")),
        "the unreadable one must still be visible as unknown, not dropped: {:?}",
        route.middleware
    );
}

#[test]
fn a_dynamic_verb_list_produces_no_route_at_all() {
    let extraction = extract_routes(&fixture("dynamic.php"));

    // We would not know which methods it answers, so there is no honest route to emit.
    assert!(
        !extraction.routes.iter().any(|r| matches!(r.method, HttpMethod::Match(_))),
        "a Route::match with a variable verb list must not become a route"
    );
    assert!(
        extraction.unresolved.iter().any(|u| u.source.contains("Route::match($verbs")),
        "...but it must be reported as unresolved: {:?}",
        extraction.unresolved
    );
}

#[test]
fn a_dynamic_resource_controller_yields_one_unresolved_not_seven_wrong_routes() {
    let extraction = extract_routes(&fixture("dynamic.php"));

    assert!(
        !extraction.routes.iter().any(|r| matches!(r.method, HttpMethod::Resource(_))),
        "expanding a resource with an unknown controller invents seven lies"
    );
    assert!(
        extraction.unresolved.iter().any(|u| u.source.contains("Route::resource('things'")),
        "the registration must still be reported: {:?}",
        extraction.unresolved
    );
}

#[test]
fn a_group_whose_callback_is_a_variable_is_reported_unresolved() {
    let extraction = extract_routes(&fixture("dynamic.php"));
    assert!(
        extraction.unresolved.iter().any(|u| u.source.contains("$deferredRoutes")),
        "an unenterable group hides an unknown number of routes and must be surfaced: {:?}",
        extraction.unresolved
    );
}

// ---------------------------------------------------------------------------
// The ordinary shapes.
// ---------------------------------------------------------------------------

#[test]
fn extracts_every_http_verb() {
    let routes = routes("web.php");

    assert_eq!(by_uri(&routes, "/users").method, HttpMethod::Get);
    assert_eq!(by_uri(&routes, "/webhook").method, HttpMethod::Any);
    assert_eq!(by_uri(&routes, "/cors").method, HttpMethod::Options);

    let methods: Vec<&HttpMethod> = routes
        .iter()
        .filter(|r| r.uri == Resolved::Known("/users/{user}".into()))
        .map(|r| &r.method)
        .collect();
    assert!(methods.contains(&&HttpMethod::Put));
    assert!(methods.contains(&&HttpMethod::Patch));
    assert!(methods.contains(&&HttpMethod::Delete));

    // POST /users and GET /users share a URI and must both survive.
    let users: Vec<&HttpMethod> = routes
        .iter()
        .filter(|r| r.uri == Resolved::Known("/users".into()))
        .map(|r| &r.method)
        .collect();
    assert_eq!(users.len(), 2, "two verbs on one URI are two routes");
}

#[test]
fn a_match_route_keeps_the_verbs_the_source_listed() {
    let routes = routes("web.php");
    assert_eq!(
        by_uri(&routes, "/search").method,
        HttpMethod::Match(vec!["GET".into(), "POST".into()]),
        "Route::match must not be collapsed into a verb list Laravel does not enumerate"
    );
}

#[test]
fn extracts_controller_class_and_method() {
    let routes = routes("web.php");
    assert_eq!(controller(by_uri(&routes, "/users")), ("UserController", Some("index")));
    assert_eq!(name(by_uri(&routes, "/users")), Some("users.index"));
}

#[test]
fn an_invokable_controller_has_no_method() {
    let routes = routes("web.php");
    // `None` rather than a guessed `"__invoke"`: the source does not say it, Laravel
    // infers it, and the distinction matters to anything resolving a symbol.
    assert_eq!(controller(by_uri(&routes, "/report")), ("ReportController", None));
}

#[test]
fn the_legacy_string_action_form_still_parses() {
    let routes = routes("web.php");
    assert_eq!(controller(by_uri(&routes, "/legacy")), ("LegacyController", Some("store")));
}

#[test]
fn a_fully_qualified_class_is_kept_verbatim() {
    let routes = routes("web.php");
    // Not shortened, not resolved against `use` statements — that needs cross-file
    // context this extractor deliberately does not have.
    assert_eq!(
        controller(by_uri(&routes, "/qualified")),
        ("\\App\\Http\\Controllers\\QualifiedController", Some("show"))
    );
}

#[test]
fn a_closure_action_is_recorded_as_a_closure_not_as_unknown() {
    let routes = routes("web.php");

    // The distinction the issue asks for: a closure is a real, complete answer. Marking
    // it unknown would make an honest route look like a parser failure.
    assert_eq!(by_uri(&routes, "/health").action, Resolved::Known(RouteAction::Closure));
    assert_eq!(by_uri(&routes, "/ping").action, Resolved::Known(RouteAction::Closure));
    assert_eq!(name(by_uri(&routes, "/health")), Some("health"));
}

#[test]
fn chained_middleware_is_collected_in_both_chain_orders() {
    let routes = routes("web.php");

    assert_eq!(middleware(by_uri(&routes, "/dashboard")), vec!["auth", "verified"]);
    assert_eq!(name(by_uri(&routes, "/dashboard")), Some("dashboard"));

    // `Route::middleware(...)->get(...)` puts the modifier before the verb.
    assert_eq!(middleware(by_uri(&routes, "/limited")), vec!["throttle:60,1"]);
}

#[test]
fn view_and_redirect_routes_record_their_target() {
    let routes = routes("web.php");

    assert_eq!(
        by_uri(&routes, "/welcome").action,
        Resolved::Known(RouteAction::View { template: Resolved::Known("pages.welcome".into()) })
    );
    assert_eq!(
        by_uri(&routes, "/old").action,
        Resolved::Known(RouteAction::Redirect { to: Resolved::Known("/new".into()) })
    );
    // Route::redirect answers every verb, which is what Laravel actually registers.
    assert_eq!(by_uri(&routes, "/old").method, HttpMethod::Any);
}

#[test]
fn every_route_carries_a_one_based_line() {
    let source = fixture("web.php");
    let routes = extract_routes(&source).routes;
    let lines: Vec<&str> = source.lines().collect();

    for route in &routes {
        assert!(route.line >= 1, "lines are 1-based for the editor");
        let line = lines[route.line - 1];
        assert!(
            line.contains("Route::"),
            "line {} should be where the registration starts, but is {line:?}",
            route.line
        );
    }
}

// ---------------------------------------------------------------------------
// Groups.
// ---------------------------------------------------------------------------

#[test]
fn group_prefix_middleware_and_name_are_inherited() {
    let routes = routes("groups.php");
    let dashboard = by_uri(&routes, "/admin/dashboard");

    assert_eq!(name(dashboard), Some("admin.dashboard"));
    assert_eq!(middleware(dashboard), vec!["auth", "can:admin"]);
}

#[test]
fn nested_groups_accumulate_prefix_middleware_and_name() {
    let routes = routes("groups.php");
    let index = by_uri(&routes, "/admin/users");

    assert_eq!(name(index), Some("admin.users.index"));
    assert_eq!(
        middleware(index),
        vec!["auth", "can:admin", "audit"],
        "outer middleware first, then inner — source order, not deduplicated"
    );

    let destroy = by_uri(&routes, "/admin/users/{user}");
    assert_eq!(name(destroy), Some("admin.users.destroy"));
    assert_eq!(destroy.method, HttpMethod::Delete);
}

#[test]
fn the_fluent_group_form_matches_the_array_form() {
    let routes = routes("groups.php");
    let status = by_uri(&routes, "/api/v1/status");

    assert_eq!(name(status), Some("api.v1.status"));
    assert_eq!(middleware(status), vec!["api", "throttle:api"]);

    // A closure inside a fluent group still inherits everything.
    let events = by_uri(&routes, "/api/v1/events");
    assert_eq!(events.action, Resolved::Known(RouteAction::Closure));
    assert_eq!(name(events), Some("api.v1.events"));
    assert_eq!(middleware(events), vec!["api", "throttle:api"]);
}

#[test]
fn a_group_without_a_prefix_does_not_invent_one() {
    let routes = routes("groups.php");

    assert_eq!(middleware(by_uri(&routes, "/unsubscribe")), vec!["signed"]);
    assert_eq!(middleware(by_uri(&routes, "/plain")), vec!["web"]);
    // No name was set anywhere in the chain, so there is no name — not an empty one.
    assert_eq!(by_uri(&routes, "/plain").name, None);
}

// ---------------------------------------------------------------------------
// Resources.
// ---------------------------------------------------------------------------

#[test]
fn a_resource_expands_to_laravels_seven_routes() {
    let routes = routes("resources.php");
    let posts: Vec<&Route> =
        routes.iter().filter(|r| name(r).is_some_and(|n| n.starts_with("posts."))).collect();

    assert_eq!(posts.len(), 7, "Laravel's ResourceRegistrar generates exactly seven");

    let names: Vec<&str> = posts.iter().filter_map(|r| name(r)).collect();
    for expected in [
        "posts.index",
        "posts.create",
        "posts.store",
        "posts.show",
        "posts.edit",
        "posts.update",
        "posts.destroy",
    ] {
        assert!(names.contains(&expected), "missing {expected} from {names:?}");
    }

    let show = posts.iter().find(|r| name(r) == Some("posts.show")).unwrap();
    assert_eq!(show.uri, Resolved::Known("/posts/{id}".into()));
    assert_eq!(controller(show), ("PostController", Some("show")));
    assert_eq!(show.method, HttpMethod::Resource("GET"));
}

#[test]
fn an_api_resource_omits_the_html_form_routes() {
    let routes = routes("resources.php");
    let names: Vec<&str> =
        routes.iter().filter_map(|r| name(r)).filter(|n| n.starts_with("widgets.")).collect();

    assert_eq!(names.len(), 5);
    assert!(!names.contains(&"widgets.create"), "apiResource has no create form");
    assert!(!names.contains(&"widgets.edit"), "apiResource has no edit form");
}

#[test]
fn only_and_except_narrow_a_resource_to_what_laravel_registers() {
    let routes = routes("resources.php");

    let tags: Vec<&str> =
        routes.iter().filter_map(name).filter(|n| n.starts_with("tags.")).collect();
    assert_eq!(tags.len(), 2, "->only(['index','show']) registers two routes, not seven");
    assert!(tags.contains(&"tags.index") && tags.contains(&"tags.show"));

    let files: Vec<&str> =
        routes.iter().filter_map(name).filter(|n| n.starts_with("files.")).collect();
    assert_eq!(files.len(), 6);
    assert!(!files.contains(&"files.destroy"), "->except() must drop the excluded action");
}

#[test]
fn an_unreadable_resource_filter_reports_the_full_set_rather_than_guessing() {
    let routes = routes("resources.php");
    let books: Vec<&str> =
        routes.iter().filter_map(name).filter(|n| n.starts_with("books.")).collect();

    // Hiding routes on a filter we cannot read would make working routes invisible, which
    // is the worse of the two failures. Over-reporting is the deliberate choice here.
    assert_eq!(books.len(), 7, "an unreadable ->only() must not silently hide routes");
}

#[test]
fn a_resource_inside_a_group_inherits_the_groups_context() {
    let routes = routes("resources.php");
    let index =
        routes.iter().find(|r| name(r) == Some("admin.photos.index")).expect("admin.photos.index");

    assert_eq!(index.uri, Resolved::Known("/admin/photos".into()));
    assert_eq!(middleware(index), vec!["auth"]);
}

// ---------------------------------------------------------------------------
// Degenerate input.
// ---------------------------------------------------------------------------

#[test]
fn a_file_with_no_routes_yields_nothing() {
    let extraction = extract_routes("<?php\n\nclass NotARouteFile {}\n");
    assert_eq!(extraction, Default::default());
}

#[test]
fn route_mentions_in_comments_and_strings_are_not_routes() {
    // The single strongest argument for tree-sitter over regex: all four of these match
    // a naive `Route::get\(` pattern, and none of them registers anything.
    let source = r#"<?php
// Route::get('/commented', [C::class, 'i']);
/* Route::get('/block', [C::class, 'i']); */
$doc = "Route::get('/in-a-string', ...)";
$sql = <<<SQL
    Route::get('/heredoc', [C::class, 'i']);
SQL;
Route::get('/real', [C::class, 'i']);
"#;

    let extraction = extract_routes(source);
    assert_eq!(uris(&extraction.routes), vec!["/real"], "only the real registration counts");
    assert!(extraction.unresolved.is_empty());
}

#[test]
fn broken_php_does_not_panic_and_still_finds_what_it_can() {
    // Files are parsed mid-edit. tree-sitter recovers; this pins that we do too.
    let source = "<?php\nRoute::get('/ok', [C::class, 'i']);\nRoute::get('/broken', [[[\n";
    let extraction = extract_routes(source);
    assert!(uris(&extraction.routes).contains(&"/ok"));
}

#[test]
fn an_empty_file_is_not_an_error() {
    assert_eq!(extract_routes(""), Default::default());
    assert_eq!(extract_routes("<?php\n"), Default::default());
}

#[test]
fn a_non_route_static_call_is_ignored() {
    // `Cache::get` looks structurally identical to `Route::get`; only the scope differs.
    let extraction =
        extract_routes("<?php\nCache::get('/users');\nGate::define('x', fn () => true);\n");
    assert_eq!(extraction, Default::default());
}

#[test]
fn the_livewire_verb_registers_a_page_route() {
    // richas-blog registers almost every page with Route::livewire('/path', 'component')
    // — a Livewire Volt helper. Without it, the routes palette misses ~90% of the app.
    // It maps to GET (a page), the URI is the first arg, the name rides ->name() as usual.
    let source = "<?php\nRoute::livewire('/posts', 'posts')->name('posts');\nRoute::livewire('/read/{slug}', 'read-post')->name('read-post');\n";
    let routes = extract_routes(source).routes;
    assert_eq!(routes.len(), 2, "both livewire routes are found");

    let posts = by_uri(&routes, "/posts");
    assert_eq!(posts.method, HttpMethod::Get, "a livewire page is a GET");
    assert_eq!(name(posts), Some("posts"));

    let read = by_uri(&routes, "/read/{slug}");
    assert_eq!(name(read), Some("read-post"));
}
