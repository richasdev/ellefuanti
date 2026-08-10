<?php

// Every route in this file is one static analysis CANNOT resolve. The extractor must say
// so rather than guess. A wrong answer here is worse than no answer: it is a route the
// user clicks and a file the editor cannot open.

$controller = app(ControllerResolver::class)->for('users');
$uri = config('routes.legacy_uri');
$prefix = env('API_PREFIX', 'v1');

// The controller is a runtime value. The method name 'handle' is right there in the
// source, but reporting it without a class would suggest a controller we cannot name.
Route::get('/resolved', [$controller, 'handle']);

// The URI is a variable.
Route::get($uri, [LegacyController::class, 'index']);

// Concatenation: the value exists only at runtime.
Route::get('/tenant/' . $tenant . '/settings', [TenantController::class, 'edit']);

// Interpolation. "/user/{$id}" is not the string /user/{$id}.
Route::get("/user/{$id}/profile", [ProfileController::class, 'show']);

// A dynamic route name.
Route::get('/named', [NamedController::class, 'index'])->name($routeName);

// A dynamic middleware entry alongside a static one: the static one must survive.
Route::get('/mixed', [MixedController::class, 'index'])->middleware(['auth', $extraMiddleware]);

// A dynamic group prefix poisons every URI inside it.
Route::group(['prefix' => $prefix], function () {
    Route::get('/inside', [InsideController::class, 'index']);
});

// A verb list that is not a literal array.
Route::match($verbs, '/multi', [MultiController::class, 'index']);

// Registration from a loop. The extractor sees one call, not N routes, and the URI it
// sees is not a literal.
foreach (['alpha', 'beta'] as $tenant) {
    Route::get("/t/{$tenant}", [LoopController::class, 'index']);
}

// A resource whose controller is dynamic: seven wrong routes would be seven lies.
Route::resource('things', $thingController);

// A group whose callback is a variable cannot be entered.
Route::prefix('deferred')->group($deferredRoutes);
