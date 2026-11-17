<?php

declare(strict_types=1);

namespace App\Desktop;

use Native\Desktop\Menu\Items\MenuItem;

/**
 * Electron menu role not wrapped by NativePHP's RolesEnum (for example zoom).
 */
final class ElectronMenuRole extends MenuItem
{
    protected string $type = 'role';

    public function __construct(
        private readonly string $role,
        private readonly ?string $roleLabel = null,
    ) {}

    /**
     * @return array<string, mixed>
     */
    public function toArray(): array
    {
        return array_merge(parent::toArray(), [
            'role' => $this->role,
            'label' => $this->roleLabel,
        ]);
    }
}
