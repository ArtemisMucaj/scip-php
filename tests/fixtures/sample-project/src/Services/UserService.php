<?php

namespace App\Services;

use App\Models\User;
use App\Enums\Status;

class UserService
{
    public function createUser(string $name, int $age): User
    {
        $user = new User($name, $age);
        return $user;
    }

    public function getUserName(User $user): string
    {
        return $user->getName();
    }

    public function isActive(Status $status): bool
    {
        return $status === Status::Active;
    }
}
