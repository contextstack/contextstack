<?php

namespace App\Client\Exceptions;

use RuntimeException;

/** No credential: this device is not paired and the local credential file cannot be read. */
class NotConnected extends RuntimeException {}
