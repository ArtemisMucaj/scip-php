<?php

namespace App\Models;

/**
 * A user profile that wraps a User.
 */
class UserProfile
{
    public User $user;
    private string $bio;

    public function __construct(User $user, string $bio)
    {
        $this->user = $user;
        $this->bio = $bio;
    }

    public function getBio(): string
    {
        return $this->bio;
    }
}
