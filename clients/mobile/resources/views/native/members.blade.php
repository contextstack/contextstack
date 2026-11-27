@use('App\NativeComponents\Members')

<native:top-bar title="Members" back="true">
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

<native:row class="w-full h-full bg-theme-background">
    {{-- The list: always on an iPad; on a phone only while no member is chosen. --}}
    <native:scroll-view class="{{ $member ? 'hidden md:flex' : 'flex' }} h-full w-full md:w-96">
        <native:column class="w-full gap-2 p-4">
            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif
            @include('native.partials.signed-out')

            @if ($issued !== [])
                <native:text class="text-sm font-bold text-theme-on-surface">{{ $issued['kind'] === 'invite' ? 'Invite for '.$issued['name'] : 'Recovery pairing for '.$issued['name'] }}</native:text>
                <native:text class="text-sm text-theme-on-surface-variant">Shown once: send it now. It is not kept on this device.</native:text>
                @if ($issuedQr !== '')
                    <native:image ref="issued-qr" src="{{ $issuedQr }}" alt="QR code for the link" class="w-64 h-64" />
                @endif
                <native:text class="text-sm text-theme-on-surface-variant">Code</native:text>
                <native:text ref="issued-code" class="font-mono text-2xl text-theme-on-surface">{{ $issued['code'] }}</native:text>
                @if ($issued['link'] !== '')
                    <native:text class="text-sm text-theme-on-surface-variant">Link</native:text>
                    <native:text ref="issued-link" class="font-mono text-sm text-theme-on-surface">{{ $issued['link'] }}</native:text>
                @endif
                @if ($issued['expires_at'] !== '')
                    <native:text class="text-xs text-theme-on-surface-variant">Works once, until {{ $issued['expires_at'] }}.</native:text>
                @endif
                <native:button label="Done" class="w-full" @tap="done" />
                <native:divider class="w-full my-2" />
            @endif

            @foreach ($members as $id => $row)
                <native:list-item native:key="member-{{ $id }}"
                    headline="{{ (string) ($row['display_name'] ?? $id) }}"
                    supporting="{{ Members::summary($row) }}"
                    leadingIcon="{{ ($row['status'] ?? 'active') === 'disabled' ? 'person.crop.circle.badge.xmark' : 'person.crop.circle' }}"
                    trailingIcon="chevron.right"
                    @press="select('{{ $id }}')" />
            @endforeach

            @if ($loaded)
                <native:text class="mt-4 text-sm font-bold text-theme-on-surface">Invite someone</native:text>
                <native:outlined-text-input label="Their name" ref="invite-name" native:model.debounce.300ms="inviteName" />
                <native:select label="Role" :options="$this->inviteRoleChoices()" native:model="inviteRole" />
                <native:button label="Create invite" variant="primary" class="w-full" @tap="invite" />

                @if ($invites !== [])
                    <native:text class="mt-4 text-sm font-bold text-theme-on-surface">Invites</native:text>
                    @foreach ($invites as $row)
                        @php($inviteId = (string) ($row['id'] ?? ''))
                        @php($open = in_array($row['status'] ?? '', ['pending', 'open', 'active'], true))
                        <native:list-item native:key="invite-{{ $inviteId }}"
                            headline="{{ (string) ($row['display_name'] ?? $members[$row['member_id'] ?? '']['display_name'] ?? ($row['member_id'] ?? $inviteId)) }}"
                            supporting="{{ trim(Members::roleWords((string) ($row['role'] ?? '')).' · '.(string) ($row['status'] ?? '').' · until '.(string) ($row['expires_at'] ?? '—'), ' ·') }}"
                            leadingIcon="envelope"
                            :trailing-actions="$open ? [['method' => 'revokeInvite(\''.$inviteId.'\')', 'label' => 'Revoke', 'icon' => 'trash', 'role' => 'destructive']] : []" />
                    @endforeach
                @endif
            @endif
        </native:column>
    </native:scroll-view>

    {{-- The member: beside the list on an iPad; in its place on a phone. --}}
    <native:scroll-view class="{{ $member ? 'flex' : 'hidden md:flex' }} h-full flex-1 bg-theme-surface">
        @if ($member)
            @php($memberId = (string) $member['id'])
            <native:column class="w-full gap-3 p-4">
                <native:button label="Back" variant="ghost" icon="chevron.left" class="md:hidden" @tap="closeDetail" />
                <native:text ref="member-name" class="text-xl font-bold text-theme-on-surface">{{ (string) ($member['display_name'] ?? $memberId) }}</native:text>
                <native:text class="text-sm text-theme-on-surface-variant">{{ Members::summary($member) }} · {{ $memberId }}</native:text>

                <native:outlined-text-input label="Display name" ref="rename-name" native:model.debounce.300ms="renameName" />
                <native:button label="Rename" variant="secondary" class="w-full" @tap="renameMember('{{ $memberId }}')" />

                <native:text class="text-sm font-bold text-theme-on-surface">Role</native:text>
                @foreach ($this->roleChoices() as $role)
                    <native:list-item native:key="role-{{ $role }}" headline="{{ Members::roleWords($role) }}"
                        leadingIcon="{{ ($member['role'] ?? '') === $role ? 'checkmark.circle.fill' : 'circle' }}"
                        @press="changeRole('{{ $memberId }}', '{{ $role }}')" />
                @endforeach

                @if (($member['status'] ?? 'active') === 'disabled')
                    <native:button label="Reactivate" variant="secondary" class="w-full" @tap="reactivateMember('{{ $memberId }}')" />
                @elseif (($member['status'] ?? 'active') === 'pending')
                    <native:button label="Reissue invite" variant="secondary" class="w-full" @tap="reissue('{{ $memberId }}')" />
                @else
                    <native:text class="text-xs text-theme-on-surface-variant">Disabling revokes their devices and open invites and ends their streams.</native:text>
                    <native:button label="Disable" variant="destructive" class="w-full" @tap="disableMember('{{ $memberId }}')" />
                @endif

                @if ($this->can('manage_member_devices'))
                    <native:text class="mt-2 text-sm font-bold text-theme-on-surface">Devices</native:text>
                    @foreach ($this->devicesOf($member) as $device)
                        @php($deviceId = (string) ($device['id'] ?? ''))
                        @if ($deviceId === $thisDevice)
                            <native:list-item native:key="device-{{ $deviceId }}" headline="{{ (string) ($device['name'] ?? $deviceId) }}" supporting="This device" leadingIcon="ipad" />
                        @elseif (! empty($device['revoked_at']))
                            <native:list-item native:key="device-{{ $deviceId }}" headline="{{ (string) ($device['name'] ?? $deviceId) }}" supporting="Revoked" leadingIcon="xmark.circle" />
                        @else
                            <native:list-item native:key="device-{{ $deviceId }}" headline="{{ (string) ($device['name'] ?? $deviceId) }}" supporting="Paired"
                                leadingIcon="laptopcomputer.and.iphone"
                                :trailing-actions="[['method' => 'revokeDevice(\''.$deviceId.'\')', 'label' => 'Revoke', 'icon' => 'trash', 'role' => 'destructive']]" />
                        @endif
                    @endforeach
                    @if (($member['status'] ?? 'active') === 'active')
                        <native:outlined-text-input label="Recovery device name" placeholder="ipad" ref="recovery-name"
                            native:model.debounce.300ms="recoveryName" autocapitalize="none" />
                        <native:button label="Get a recovery pairing code" variant="secondary" class="w-full" @tap="recoveryPairing('{{ $memberId }}')" />
                    @endif
                @endif
            </native:column>
        @else
            <native:column class="w-full h-full items-center justify-center p-8">
                <native:text class="text-base text-theme-on-surface-variant">Choose a member to change their role, devices or access.</native:text>
            </native:column>
        @endif
    </native:scroll-view>
</native:row>
