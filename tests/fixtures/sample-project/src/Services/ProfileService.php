<?php

namespace App\Services;

use App\Models\User;
use App\Models\UserProfile;

/**
 * Service that works with UserProfile via chained property access.
 */
class ProfileService
{
    /**
     * Get the user's name via a chained property access ($profile->user->getName()).
     * This tests that the property type of UserProfile::$user is resolved to App\Models\User,
     * enabling the method call to emit a reference to User#getName().
     */
    public function getProfileUserName(UserProfile $profile): string
    {
        return $profile->user->getName();
    }

    /**
     * Get the user's name via a null-safe chained property access.
     */
    public function getProfileUserNameNullSafe(?UserProfile $profile): ?string
    {
        return $profile?->user?->getName();
    }
}
