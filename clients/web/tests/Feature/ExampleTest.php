<?php

test('the home page asks an unpaired browser to connect first', function () {
    $this->get(route('home'))->assertRedirect(route('connect'));
});
