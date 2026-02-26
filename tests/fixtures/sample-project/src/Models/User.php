<?php

namespace App\Models;

use App\Contracts\UserRepository;

/**
 * Represents a user in the system.
 *
 * @since 1.0.0
 */
class User
{
    public const MAX_NAME_LENGTH = 255;

    private string $name;
    private int $age;

    /**
     * Create a new User instance.
     *
     * @param string $name The user's name
     * @param int $age The user's age
     */
    public function __construct(string $name, int $age)
    {
        $this->name = $name;
        $this->age = $age;
    }

    /**
     * Get the user's name.
     */
    public function getName(): string
    {
        return $this->name;
    }

    public function setName(string $name): void
    {
        $this->name = $name;
    }
}
