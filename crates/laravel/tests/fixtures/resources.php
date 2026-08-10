<?php

Route::resource('posts', PostController::class);

Route::apiResource('widgets', WidgetController::class);

// A resource inside a group inherits prefix, middleware and name prefix.
Route::group(['prefix' => 'admin', 'middleware' => 'auth', 'as' => 'admin.'], function () {
    Route::resource('photos', PhotoController::class);
});

// ->only() and ->except() narrow the set Laravel actually registers.
Route::resource('tags', TagController::class)->only(['index', 'show']);
Route::resource('files', FileController::class)->except(['destroy']);

// An unreadable filter must not be guessed at.
Route::resource('books', BookController::class)->only($allowedActions);
