//! Route extraction from `routes/*.php`.
//!
//! Walks the tree-sitter PHP tree looking for `Route::<verb>(...)` calls and the chained
//! `->name()` / `->middleware()` / `->prefix()` that decorate them, plus `Route::group()`
//! nesting. Deliberately *not* regex: `Route::get` inside a comment, a string, or a
//! heredoc is indistinguishable to a regex, and a route panel that navigates to a
//! commented-out line is precisely the confident wrongness RISKS.md #4 warns about.
//!
//! What this cannot see, by construction: routes registered in service providers, through
//! macros, from config loops, or by packages. That is a known and accepted gap — see the
//! module docs on [`crate`].

use elle_syntax::{Language, SyntaxTree};
use elle_text::Buffer;
use tree_sitter::Node;

use crate::resolved::Resolved;

/// The HTTP verb a route responds to.
///
/// `Any` and `Match` are kept distinct from a list of verbs because they are what the
/// source said; collapsing `Route::any` into all seven verbs would invent route entries
/// Laravel itself does not enumerate that way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Options,
    /// `Route::any(...)` — responds to every verb.
    Any,
    /// `Route::match([...], ...)` — the listed verbs, uppercased as written.
    Match(Vec<String>),
    /// The verbs a `Route::resource` entry maps to. Laravel's own mapping, not a guess:
    /// see [`RESOURCE_ROUTES`].
    Resource(&'static str),
}

impl HttpMethod {
    /// The verb from a `Route::<name>` call, or `None` if `name` is not a routing verb.
    fn from_verb(name: &str) -> Option<Self> {
        Some(match name {
            "get" => HttpMethod::Get,
            "post" => HttpMethod::Post,
            "put" => HttpMethod::Put,
            "patch" => HttpMethod::Patch,
            "delete" => HttpMethod::Delete,
            "options" => HttpMethod::Options,
            "any" => HttpMethod::Any,
            _ => return None,
        })
    }
}

/// What handles a route.
///
/// Closures are a first-class answer, not a failure: `Route::get('/', fn () => view('x'))`
/// is ordinary Laravel. That is why this is distinct from [`Resolved::Unknown`], which
/// means "there is an action here and we could not read it".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteAction {
    /// `[UserController::class, 'index']`, `'UserController@index'`, or an invokable
    /// `UserController::class`. `method` is `None` for an invokable controller, where
    /// Laravel calls `__invoke`.
    Controller { class: String, method: Option<String> },
    /// A closure or arrow function defined inline. Nothing to navigate to but the line
    /// itself, which the route's own `line` already gives.
    Closure,
    /// `Route::view('/x', 'welcome')` — handled by the framework, no user code.
    View { template: Resolved<String> },
    /// `Route::redirect('/a', '/b')`.
    Redirect { to: Resolved<String> },
}

/// One statically-derived route registration.
///
/// Every field that could be dynamic is wrapped in [`Resolved`]. `middleware` is not: an
/// unresolvable middleware expression is recorded as an `Unknown` *element*, so a route
/// with `->middleware(['auth', $extra])` keeps the `auth` it does know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub method: HttpMethod,
    /// Full URI including any prefix inherited from enclosing groups, normalised to a
    /// single leading slash.
    pub uri: Resolved<String>,
    /// The route's name, including any group name prefix. `None` means the source never
    /// called `->name()` — distinct from `Some(Unknown)`, which means it did but with an
    /// expression we could not read.
    pub name: Option<Resolved<String>>,
    pub action: Resolved<RouteAction>,
    /// Group middleware first, then the route's own, in source order. Duplicates are kept:
    /// deduplicating would misrepresent what the file says.
    pub middleware: Vec<Resolved<String>>,
    /// 1-based, matching what an editor shows and what a "go to definition" needs.
    pub line: usize,
}

/// Routes found in one file, plus what defeated the extractor in it.
///
/// The second half is not decoration. A caller that shows "12 routes" when the file has a
/// `foreach` registering 40 more is lying by omission; `unresolved` is how it can say
/// "12 routes, 3 registrations not statically readable" instead.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteExtraction {
    pub routes: Vec<Route>,
    /// Source text of `Route::` calls recognised as registrations but not resolvable into
    /// a route at all — a dynamic verb, or a group whose body we could not enter.
    pub unresolved: Vec<UnresolvedRegistration>,
}

/// A route registration we saw but could not turn into a [`Route`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnresolvedRegistration {
    /// The source text, trimmed, so the user can see what we choked on.
    pub source: String,
    pub line: usize,
}

/// The seven routes `Route::resource` generates, as Laravel defines them.
///
/// Hard-coded from the framework's own `ResourceRegistrar`, not inferred. `->only()` and
/// `->except()` filtering is *not* applied — see the note in [`extract_routes`].
const RESOURCE_ROUTES: [(&str, &str, &str); 7] = [
    // (action method, verb, URI suffix)
    ("index", "GET", ""),
    ("create", "GET", "/create"),
    ("store", "POST", ""),
    ("show", "GET", "/{id}"),
    ("edit", "GET", "/{id}/edit"),
    ("update", "PUT", "/{id}"),
    ("destroy", "DELETE", "/{id}"),
];

/// The API subset — `Route::apiResource` omits the HTML form routes.
const API_RESOURCE_ACTIONS: [&str; 5] = ["index", "store", "show", "update", "destroy"];

/// Context inherited from enclosing `Route::group()` calls.
#[derive(Clone, Default)]
struct GroupContext {
    /// Concatenated prefixes, outermost first, each already trimmed of slashes.
    prefix: Vec<Resolved<String>>,
    middleware: Vec<Resolved<String>>,
    /// `->name('admin.')` on a group prefixes member route names. Laravel does not insert
    /// a separator, so these concatenate verbatim.
    name_prefix: Vec<Resolved<String>>,
}

/// Extracts every statically visible route registration from one PHP file.
///
/// `path` is only used to stamp results; nothing is read from disk. The caller supplies
/// the source, which keeps this usable against an unsaved buffer as well as a file.
///
/// `Route::resource(...)` is expanded to the routes Laravel's own `ResourceRegistrar`
/// generates, narrowed by `->only()` / `->except()` when those are statically readable.
/// When they are not, the full set is reported rather than a guessed subset: an extra
/// entry that 404s is a milder failure than hiding a route that works.
///
/// ponytail: single-file. Cross-file resolution (a controller's `use` statements, a group
/// whose callback is a named function elsewhere) needs the project index from #21, which
/// does not exist yet. Class names come back exactly as written in the source — short
/// name or fully qualified — and resolving them is that follow-up's job.
pub fn extract_routes(source: &str) -> RouteExtraction {
    let buffer = Buffer::new(source);
    // Reuses the same parse path the editor uses (ADR-0005) rather than opening a second
    // route into tree-sitter, so a grammar upgrade moves both together.
    let Ok(tree) = SyntaxTree::new(Language::Php, &buffer) else {
        return RouteExtraction::default();
    };
    let Some(tree) = tree.tree() else {
        return RouteExtraction::default();
    };

    let mut extraction = RouteExtraction::default();
    visit(tree.root_node(), source.as_bytes(), &GroupContext::default(), &mut extraction);
    extraction
}

/// Walks a subtree, handling any route registration it finds and recursing otherwise.
fn visit(node: Node, src: &[u8], group: &GroupContext, out: &mut RouteExtraction) {
    // Only statement-level expressions register routes. Starting from the statement means
    // the whole `->name()->middleware()` chain is in hand at once, which is the only way
    // to read a chain that nests object-inward.
    if node.kind() == "expression_statement"
        && let Some(expr) = node.named_child(0)
        && is_route_chain(expr, src)
    {
        handle_chain(expr, src, group, out);
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child, src, group, out);
    }
}

/// Whether this expression chain bottoms out in a `Route::` static call.
///
/// Walks the `object:` spine inward: `Route::get(..)->name(..)` is
/// `member_call(member_call(scoped_call))`, so the `Route` scope is at the deepest level.
fn is_route_chain(node: Node, src: &[u8]) -> bool {
    chain_root(node).is_some_and(|root| {
        root.child_by_field_name("scope")
            .and_then(|scope| text(scope, src))
            // `Route` is the facade; `Router` covers `$router`-style registrars only when
            // written statically. An aliased import (`use Illuminate\Support\Facades\Route
            // as R`) is not followed — that needs cross-file resolution.
            .is_some_and(|scope| scope.trim_start_matches('\\').ends_with("Route"))
    })
}

/// The innermost `scoped_call_expression` of a `->`-chain, or `None` if it is not one.
fn chain_root(mut node: Node) -> Option<Node> {
    loop {
        match node.kind() {
            "scoped_call_expression" => return Some(node),
            "member_call_expression" => node = node.child_by_field_name("object")?,
            _ => return None,
        }
    }
}

/// Turns one `Route::...` statement chain into routes.
fn handle_chain(expr: Node, src: &[u8], group: &GroupContext, out: &mut RouteExtraction) {
    // Collect the chain outermost-to-innermost, then reverse: source order is what the
    // user reads, and `->name('a')->name('b')` should behave like Laravel (last wins)
    // rather than like our traversal order.
    let mut links = Vec::new();
    let mut node = expr;
    loop {
        match node.kind() {
            "member_call_expression" => {
                links.push(node);
                let Some(object) = node.child_by_field_name("object") else { return };
                node = object;
            }
            "scoped_call_expression" => {
                links.push(node);
                break;
            }
            _ => return,
        }
    }
    links.reverse();

    // Modifiers apply to whatever the chain ends up being; gather them before dispatch so
    // `Route::middleware('auth')->get(...)` and `Route::get(...)->middleware('auth')` land
    // in the same place.
    let mut local = LocalModifiers::default();
    for link in &links {
        let Some(name) = link.child_by_field_name("name").and_then(|n| text(n, src)) else {
            continue;
        };
        let args = link.child_by_field_name("arguments");
        match name {
            "name" => local.name = args.and_then(|a| arg(a, 0)).map(|n| string_value(n, src)),
            "middleware" => {
                if let Some(node) = args.and_then(|a| arg(a, 0)) {
                    local.middleware.extend(string_list(node, src));
                }
            }
            "prefix" => {
                if let Some(node) = args.and_then(|a| arg(a, 0)) {
                    local.prefix = Some(string_value(node, src));
                }
            }
            // Resource filters. Only the statically readable entries are kept: an
            // unreadable filter leaves `None`, which falls back to the full set rather
            // than silently hiding routes.
            "only" | "except" => {
                if let Some(node) = args.and_then(|a| arg(a, 0)) {
                    let actions: Vec<String> = string_list(node, src)
                        .into_iter()
                        .filter_map(|a| a.known().cloned())
                        .collect();
                    if !actions.is_empty() {
                        local.resource_filter = Some((name == "only", actions));
                    }
                }
            }
            _ => {}
        }
    }

    // A `->group()` anywhere in the chain makes this a group, whatever the root verb was.
    if let Some(group_link) = links
        .iter()
        .find(|l| l.child_by_field_name("name").and_then(|n| text(n, src)) == Some("group"))
    {
        handle_group(*group_link, src, group, &local, out);
        return;
    }

    // The verb is not necessarily the chain root: `Route::middleware('auth')->get(...)`
    // registers exactly what `Route::get(...)->middleware('auth')` does, and the verb is
    // the *outer* call there. So find the link that names a registration, wherever it sits.
    let verb_link = links.iter().find(|link| {
        link.child_by_field_name("name")
            .and_then(|n| text(n, src))
            .is_some_and(is_registration_verb)
    });

    let Some(&root) = verb_link else {
        // A chain of pure modifiers with no verb and no `group()` registers nothing —
        // it is either dead code or a builder handed to something else.
        let root = links[0];
        let name = root.child_by_field_name("name").and_then(|n| text(n, src)).unwrap_or_default();
        // `Route::fallback`, `Route::macro`, a package's own verb: recorded rather than
        // silently dropped, so a caller can tell "no routes here" from "routes we could
        // not read".
        if !MODIFIERS.contains(&name) {
            out.unresolved.push(unresolved(root, src));
        }
        return;
    };

    let method_name =
        root.child_by_field_name("name").and_then(|n| text(n, src)).unwrap_or_default();

    match method_name {
        "resource" | "apiResource" => {
            handle_resource(root, src, group, &local, method_name == "apiResource", out)
        }
        "view" | "redirect" => handle_framework_route(root, src, group, &local, method_name, out),
        "match" => handle_match(root, src, group, &local, out),
        // `is_registration_verb` already vetted this, so the fallback is unreachable.
        _ => {
            if let Some(method) = HttpMethod::from_verb(method_name) {
                handle_verb(root, src, group, &local, method, out);
            }
        }
    }
}

/// Chain links that only decorate a registration rather than being one.
const MODIFIERS: [&str; 10] = [
    "middleware",
    "prefix",
    "name",
    "domain",
    "where",
    "withoutMiddleware",
    "scopeBindings",
    "as",
    "only",
    "except",
];

/// Whether a `Route::`/`->` call name registers a route.
fn is_registration_verb(name: &str) -> bool {
    HttpMethod::from_verb(name).is_some()
        || matches!(name, "resource" | "apiResource" | "view" | "redirect" | "match")
}

/// Modifiers read off one chain, before group context is merged in.
#[derive(Default)]
struct LocalModifiers {
    name: Option<Resolved<String>>,
    middleware: Vec<Resolved<String>>,
    prefix: Option<Resolved<String>>,
    /// `->only([...])` / `->except([...])` on a resource. `true` means `only`.
    resource_filter: Option<(bool, Vec<String>)>,
}

/// `Route::get('/uri', action)` and friends.
fn handle_verb(
    root: Node,
    src: &[u8],
    group: &GroupContext,
    local: &LocalModifiers,
    method: HttpMethod,
    out: &mut RouteExtraction,
) {
    let Some(args) = root.child_by_field_name("arguments") else { return };
    let uri = match arg(args, 0) {
        Some(node) => string_value(node, src),
        None => return,
    };
    let action = match arg(args, 1) {
        Some(node) => action_value(node, src),
        // `Route::get('/x')` with no action is not valid Laravel; treat the action as
        // unreadable rather than inventing a closure.
        None => Resolved::Unknown(String::new()),
    };

    out.routes.push(build(method, uri, action, root, group, local));
}

/// `Route::match(['get', 'post'], '/uri', action)`.
fn handle_match(
    root: Node,
    src: &[u8],
    group: &GroupContext,
    local: &LocalModifiers,
    out: &mut RouteExtraction,
) {
    let Some(args) = root.child_by_field_name("arguments") else { return };
    let Some(verbs_node) = arg(args, 0) else { return };

    // A dynamic verb list makes the whole entry a guess: we would not know which methods
    // it answers. Report it as unresolved rather than picking one.
    let verbs: Vec<String> = string_list(verbs_node, src)
        .into_iter()
        .filter_map(|v| v.known().map(|s| s.to_ascii_uppercase()))
        .collect();
    if verbs.is_empty() {
        out.unresolved.push(unresolved(root, src));
        return;
    }

    let uri = match arg(args, 1) {
        Some(node) => string_value(node, src),
        None => return,
    };
    let action = arg(args, 2).map_or(Resolved::Unknown(String::new()), |n| action_value(n, src));

    out.routes.push(build(HttpMethod::Match(verbs), uri, action, root, group, local));
}

/// `Route::view('/uri', 'template')` and `Route::redirect('/from', '/to')`.
fn handle_framework_route(
    root: Node,
    src: &[u8],
    group: &GroupContext,
    local: &LocalModifiers,
    verb: &str,
    out: &mut RouteExtraction,
) {
    let Some(args) = root.child_by_field_name("arguments") else { return };
    let uri = match arg(args, 0) {
        Some(node) => string_value(node, src),
        None => return,
    };
    let target = arg(args, 1).map_or(Resolved::Unknown(String::new()), |n| string_value(n, src));

    let (method, action) = match verb {
        "view" => (HttpMethod::Get, RouteAction::View { template: target }),
        // `Route::redirect` answers every verb in Laravel, which is why this is not Get.
        _ => (HttpMethod::Any, RouteAction::Redirect { to: target }),
    };

    out.routes.push(build(method, uri, Resolved::Known(action), root, group, local));
}

/// `Route::resource('posts', PostController::class)` — expands to Laravel's seven.
fn handle_resource(
    root: Node,
    src: &[u8],
    group: &GroupContext,
    local: &LocalModifiers,
    api_only: bool,
    out: &mut RouteExtraction,
) {
    let Some(args) = root.child_by_field_name("arguments") else { return };
    let (Some(base_node), Some(class_node)) = (arg(args, 0), arg(args, 1)) else { return };

    let base = string_value(base_node, src);
    let class = class_name(class_node, src);

    // A dynamic base URI or controller would make every expanded entry a guess. One
    // unresolved registration is more honest than a fistful of wrong routes.
    let (Resolved::Known(base), Resolved::Known(class)) = (&base, &class) else {
        out.unresolved.push(unresolved(root, src));
        return;
    };

    // The resource name doubles as the route-name stem: `posts.index`, and for a nested
    // `photos.comments` the URI segments are already in the string Laravel was given.
    for (action, verb, suffix) in RESOURCE_ROUTES {
        if api_only && !API_RESOURCE_ACTIONS.contains(&action) {
            continue;
        }
        // `->only([...])` / `->except([...])`. Applying this can only ever *remove* an
        // entry the framework does not register, so it moves toward the truth; an
        // unreadable filter is `None` here and leaves the full set rather than guessing
        // which routes to hide.
        if let Some((is_only, actions)) = &local.resource_filter
            && actions.iter().any(|a| a == action) != *is_only
        {
            continue;
        }

        let uri = Resolved::Known(format!("{base}{suffix}"));
        let route_action = Resolved::Known(RouteAction::Controller {
            class: class.clone(),
            method: Some(action.to_string()),
        });

        let mut route = build(
            HttpMethod::Resource(verb),
            uri,
            route_action,
            root,
            group,
            // The chain's own `->name()` names the *resource*, not each route, so it is
            // handled below rather than passed through as a route name.
            &LocalModifiers {
                name: None,
                middleware: local.middleware.clone(),
                prefix: local.prefix.clone(),
                resource_filter: None,
            },
        );

        route.name = Some(join_name(&group.name_prefix, &format!("{base}.{action}")));
        out.routes.push(route);
    }
}

/// `Route::group([...], function () { ... })` — recurses into the callback with merged
/// context.
fn handle_group(
    group_link: Node,
    src: &[u8],
    outer: &GroupContext,
    local: &LocalModifiers,
    out: &mut RouteExtraction,
) {
    let Some(args) = group_link.child_by_field_name("arguments") else { return };

    let mut inner = outer.clone();
    inner.middleware.extend(local.middleware.iter().cloned());
    if let Some(prefix) = &local.prefix {
        inner.prefix.push(prefix.clone());
    }
    if let Some(name) = &local.name {
        inner.name_prefix.push(name.clone());
    }

    // The attribute-array form: `Route::group(['prefix' => 'admin', ...], fn)`.
    let mut body = arg(args, 0);
    if let Some(first) = arg(args, 0)
        && first.kind() == "array_creation_expression"
    {
        apply_group_array(first, src, &mut inner);
        body = arg(args, 1);
    }

    let Some(body) = body else { return };
    match body.kind() {
        "anonymous_function" | "arrow_function" => {
            if let Some(inner_body) = body.child_by_field_name("body") {
                visit(inner_body, src, &inner, out);
            }
        }
        // `Route::group(base_path('routes/extra.php'))` loads another file, and
        // `->group($callback)` takes a variable. Neither is followable from here.
        _ => out.unresolved.push(unresolved(group_link, src)),
    }
}

/// Reads `prefix`, `middleware` and `as` out of a group's attribute array.
fn apply_group_array(array: Node, src: &[u8], ctx: &mut GroupContext) {
    let mut cursor = array.walk();
    for element in array.named_children(&mut cursor) {
        if element.kind() != "array_element_initializer" {
            continue;
        }
        // `'prefix' => 'admin'` is two named children with no field names, so position is
        // the only handle the grammar gives us.
        let (Some(key), Some(value)) = (element.named_child(0), element.named_child(1)) else {
            continue;
        };
        let Some(Resolved::Known(key)) = Some(string_value(key, src)) else { continue };

        match key.as_str() {
            "prefix" => ctx.prefix.push(string_value(value, src)),
            "middleware" => ctx.middleware.extend(string_list(value, src)),
            // Laravel spells a group's name prefix `as`.
            "as" => ctx.name_prefix.push(string_value(value, src)),
            _ => {}
        }
    }
}

/// Assembles a route from its parts plus inherited group context.
fn build(
    method: HttpMethod,
    uri: Resolved<String>,
    action: Resolved<RouteAction>,
    root: Node,
    group: &GroupContext,
    local: &LocalModifiers,
) -> Route {
    let mut middleware = group.middleware.clone();
    middleware.extend(local.middleware.iter().cloned());

    let mut prefixes = group.prefix.clone();
    if let Some(prefix) = &local.prefix {
        prefixes.push(prefix.clone());
    }

    Route {
        method,
        uri: join_uri(&prefixes, &uri),
        name: local.name.as_ref().map(|n| match n {
            Resolved::Known(name) => join_name(&group.name_prefix, name),
            // An unreadable name stays unreadable no matter what prefixes it.
            Resolved::Unknown(source) => Resolved::Unknown(source.clone()),
        }),
        action,
        middleware,
        // tree-sitter rows are 0-based; editors are not.
        line: root.start_position().row + 1,
    }
}

/// Joins group prefixes and a URI into one path.
///
/// Any unknown segment poisons the whole result: a URI with a hole in the middle is not a
/// URI, and reporting `/admin/{unknown}/edit` as navigable would be a lie.
fn join_uri(prefixes: &[Resolved<String>], uri: &Resolved<String>) -> Resolved<String> {
    let mut parts = Vec::new();
    for segment in prefixes.iter().chain(std::iter::once(uri)) {
        match segment {
            Resolved::Known(text) => {
                let trimmed = text.trim_matches('/');
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
            Resolved::Unknown(source) => return Resolved::Unknown(source.clone()),
        }
    }
    Resolved::Known(format!("/{}", parts.join("/")))
}

/// Concatenates group name prefixes with a route name. Laravel inserts no separator —
/// `->name('admin.')` includes its own dot — so neither do we.
fn join_name(prefixes: &[Resolved<String>], name: &str) -> Resolved<String> {
    let mut out = String::new();
    for prefix in prefixes {
        match prefix {
            Resolved::Known(text) => out.push_str(text),
            Resolved::Unknown(source) => return Resolved::Unknown(source.clone()),
        }
    }
    out.push_str(name);
    Resolved::Known(out)
}

/// Reads a PHP expression as a literal string, or records why it could not be.
///
/// Interpolated strings, concatenations and variables all land in `Unknown`: `"/x/{$id}"`
/// has a value only at runtime, and `/x/{$id}` is not it.
fn string_value(node: Node, src: &[u8]) -> Resolved<String> {
    if node.kind() == "string"
        && let Some(content) = node.named_child(0)
        && content.kind() == "string_content"
    {
        return text(content, src).map_or_else(
            || Resolved::Unknown(source_text(node, src)),
            |t| Resolved::Known(t.to_string()),
        );
    }
    // An empty single-quoted string has no `string_content` child at all.
    if node.kind() == "string" && node.named_child_count() == 0 {
        return Resolved::Known(String::new());
    }
    Resolved::Unknown(source_text(node, src))
}

/// Reads a string, or an array of strings, as a list. A bare string is a one-element list,
/// which is what `->middleware('auth')` means.
fn string_list(node: Node, src: &[u8]) -> Vec<Resolved<String>> {
    if node.kind() != "array_creation_expression" {
        return vec![string_value(node, src)];
    }

    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| c.kind() == "array_element_initializer")
        .filter_map(|element| element.named_child(0))
        .map(|value| string_value(value, src))
        .collect()
}

/// Reads a route's second argument as an action.
fn action_value(node: Node, src: &[u8]) -> Resolved<RouteAction> {
    match node.kind() {
        "anonymous_function" | "arrow_function" => Resolved::Known(RouteAction::Closure),

        // `[UserController::class, 'index']`.
        "array_creation_expression" => {
            let mut cursor = node.walk();
            let parts: Vec<Node> = node
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "array_element_initializer")
                .filter_map(|e| e.named_child(0))
                .collect();

            let [class_node, method_node] = parts[..] else {
                return Resolved::Unknown(source_text(node, src));
            };
            match (class_name(class_node, src), string_value(method_node, src)) {
                (Resolved::Known(class), Resolved::Known(method)) => {
                    Resolved::Known(RouteAction::Controller { class, method: Some(method) })
                }
                // `[$controller, 'act']` — the class is a runtime value. Reporting the
                // method alone would suggest a controller we cannot name.
                _ => Resolved::Unknown(source_text(node, src)),
            }
        }

        // An invokable controller: `Route::get('/x', ReportController::class)`.
        "class_constant_access_expression" => match class_name(node, src) {
            Resolved::Known(class) => {
                Resolved::Known(RouteAction::Controller { class, method: None })
            }
            Resolved::Unknown(source) => Resolved::Unknown(source),
        },

        // The legacy `'UserController@index'` string form.
        "string" => match string_value(node, src) {
            Resolved::Known(text) => match text.split_once('@') {
                Some((class, method)) => Resolved::Known(RouteAction::Controller {
                    class: class.to_string(),
                    method: Some(method.to_string()),
                }),
                // A bare string action is an invokable class name in older Laravel.
                None if !text.is_empty() => {
                    Resolved::Known(RouteAction::Controller { class: text, method: None })
                }
                _ => Resolved::Unknown(source_text(node, src)),
            },
            Resolved::Unknown(source) => Resolved::Unknown(source),
        },

        _ => Resolved::Unknown(source_text(node, src)),
    }
}

/// Reads `Foo::class` or `\App\Foo::class` as a class name, verbatim as written.
fn class_name(node: Node, src: &[u8]) -> Resolved<String> {
    if node.kind() == "class_constant_access_expression"
        && let Some(name) = node.named_child(0)
        && matches!(name.kind(), "name" | "qualified_name")
    {
        return text(name, src).map_or_else(
            || Resolved::Unknown(source_text(node, src)),
            |t| Resolved::Known(t.into()),
        );
    }
    Resolved::Unknown(source_text(node, src))
}

/// The `index`-th `argument` child of an `arguments` node.
fn arg(args: Node, index: usize) -> Option<Node> {
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .filter(|c| c.kind() == "argument")
        .nth(index)
        .and_then(|a| a.named_child(0))
}

fn text<'a>(node: Node, src: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(src).ok()
}

fn source_text(node: Node, src: &[u8]) -> String {
    text(node, src).unwrap_or_default().trim().to_string()
}

fn unresolved(node: Node, src: &[u8]) -> UnresolvedRegistration {
    UnresolvedRegistration { source: source_text(node, src), line: node.start_position().row + 1 }
}
