<?php

namespace App\Client\Exceptions;

use RuntimeException;

/**
 * The Director could not be reached, or its answer was lost. A write that ends here may have been recorded, so the
 * page keeps its Idempotency-Key and a retry replays it (CLIENT-API.md §7.1).
 */
class DirectorUnavailable extends RuntimeException {}
