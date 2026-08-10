<?php

// Group nesting: prefix, middleware and name must all be inherited.

Route::group(['prefix' => 'admin', 'middleware' => ['auth', 'can:admin'], 'as' => 'admin.'], function () {
    Route::get('/dashboard', [AdminController::class, 'index'])->name('dashboard');

    // Nested one level deeper: prefixes and middleware accumulate.
    Route::group(['prefix' => 'users', 'middleware' => 'audit', 'as' => 'users.'], function () {
        Route::get('/', [AdminUserController::class, 'index'])->name('index');
        Route::delete('/{user}', [AdminUserController::class, 'destroy'])->name('destroy');
    });
});

// The fluent form is the same thing written differently, so it must produce the same
// shape of result.
Route::prefix('api/v1')->middleware(['api', 'throttle:api'])->name('api.v1.')->group(function () {
    Route::get('/status', [StatusController::class, 'show'])->name('status');
    Route::post('/events', function () {
        return 204;
    })->name('events');
});

// A group's own middleware applies even when the member route adds none.
Route::middleware('signed')->group(function () {
    Route::get('/unsubscribe', [MailController::class, 'unsubscribe']);
});

// A group with no prefix must not invent one.
Route::group(['middleware' => 'web'], function () {
    Route::get('/plain', [PlainController::class, 'index']);
});
