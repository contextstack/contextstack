A rules-tier commit while scripts/check is red is refused unless the
subject starts with check-red: and names the parsed FAIL lines it is
not responsible for. Unparseable reds cannot be excepted. Authorization
is recorded before git; a landed event is written only after HEAD moves.
