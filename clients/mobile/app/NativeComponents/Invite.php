<?php

namespace App\NativeComponents;

use App\Native\PairingLink;

/**
 * The screen a contextstack://invite link opens: Connect, reading the link as an invite.
 */
class Invite extends Connect
{
    /** @var 'pair'|'invite' */
    protected string $linkKind = PairingLink::Invite;
}
