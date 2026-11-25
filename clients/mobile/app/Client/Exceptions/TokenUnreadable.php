<?php

namespace App\Client\Exceptions;

/**
 * The device is paired (its credential id is saved) but the Keychain did not give its token just now. That is not
 * "unpaired": sending the owner to pair again would leave a second live credential on the Director. The screen says
 * so and the next refresh tries again.
 */
class TokenUnreadable extends DirectorUnavailable {}
