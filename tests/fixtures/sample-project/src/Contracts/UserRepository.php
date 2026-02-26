<?php

namespace App\Contracts;

interface UserRepository
{
    public function find(int $id): ?User;
    public function save(User $user): void;
}
