<?php

namespace App\Livewire\Forms;

use App\Client\DirectorClient;
use Livewire\Attributes\Validate;
use Livewire\Form;

/**
 * A project's or conversation's budget as entered (CLIENT-API.md §12.2): dollars of actual pay-as-you-go spending,
 * per total, day or month. A budget write replaces the budget, so it carries no Idempotency-Key, and any client
 * credential may make it; the answer is the record with its new budget.
 */
class BudgetForm extends Form
{
    #[Validate('required|numeric|min:0|max:1000000')]
    public string $usd = '';

    #[Validate('required|in:total,day,month')]
    public string $period = 'total';

    /**
     * PUT {path} {usd, period}, once the entry is valid.
     *
     * @return array<string, mixed>
     */
    public function put(DirectorClient $director, string $path): array
    {
        $this->validate();

        return $director->replace($path, ['usd' => $this->usd, 'period' => $this->period]);
    }

    /**
     * DELETE {path}: no budget, so only the source budgets apply.
     *
     * @return array<string, mixed>
     */
    public function clear(DirectorClient $director, string $path): array
    {
        return $director->replace($path, null);
    }
}
