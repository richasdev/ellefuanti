<?php

// The ordinary shapes, as they appear in a real routes/web.php.

Route::get('/users', [UserController::class, 'index'])->name('users.index');
Route::post('/users', [UserController::class, 'store'])->name('users.store');
Route::put('/users/{user}', [UserController::class, 'update']);
Route::patch('/users/{user}', [UserController::class, 'patch']);
Route::delete('/users/{user}', [UserController::class, 'destroy']);
Route::any('/webhook', [WebhookController::class, 'handle']);
Route::match(['get', 'post'], '/search', [SearchController::class, 'run'])->name('search');
Route::options('/cors', [CorsController::class, 'preflight']);

// A closure action is legitimate Laravel, not a failure to resolve.
Route::get('/health', function () {
    return response()->json(['ok' => true]);
})->name('health');

Route::get('/ping', fn () => 'pong');

// An invokable controller has no method: Laravel calls __invoke.
Route::get('/report', ReportController::class)->name('report');

// The legacy string form.
Route::post('/legacy', 'LegacyController@store');

// Fully qualified, which is how a file with no `use` statement writes it.
Route::get('/qualified', [\App\Http\Controllers\QualifiedController::class, 'show']);

// Chained modifiers, in both orders.
Route::get('/dashboard', [DashboardController::class, 'index'])
    ->name('dashboard')
    ->middleware(['auth', 'verified']);

Route::middleware('throttle:60,1')->get('/limited', [LimitedController::class, 'index']);

// Framework-handled routes.
Route::view('/welcome', 'pages.welcome');
Route::redirect('/old', '/new');
