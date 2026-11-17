<?php

use App\Client\Money;

it('shows dollars the way the Director does, honest for small amounts', function (mixed $value, string $shown) {
    expect(Money::dollars($value))->toBe($shown);
})->with([
    'nothing' => [0, '$0.00'],
    'whole cents' => [1.25, '$1.25'],
    'a decimal string' => ['2.5', '$2.50'],
    'two cents' => [0.02, '$0.02'],
    'two cents less a little' => [0.018458, '$0.0184'],
    'a fraction of a cent' => [0.001542, '$0.00154'],
    'twenty cents less a little' => [0.1983, '$0.198'],
    'a millionth' => [0.000001, '$0.000001'],
    'never rounded up to the whole' => [4.999999, '$4.9999'],
    'over a dollar' => [12.345678, '$12.3456'],
    'negative' => [-1, '$0.00'],
]);

it('shows a dash for no amount', function (mixed $value) {
    expect(Money::dollars($value))->toBe('—');
})->with([null, 'n/a', [[]]]);
