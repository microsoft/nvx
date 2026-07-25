// Pin the parent guest shell to vCPU 1 using Linux x86-64 syscalls only.

static long syscall0(long number)
{
    long result;
    __asm__ volatile("syscall"
                     : "=a"(result)
                     : "a"(number)
                     : "rcx", "r11", "memory");
    return result;
}

static long syscall1(long number, long argument)
{
    long result;
    __asm__ volatile("syscall"
                     : "=a"(result)
                     : "a"(number), "D"(argument)
                     : "rcx", "r11", "memory");
    return result;
}

static long syscall3(long number, long argument1, long argument2, long argument3)
{
    long result;
    __asm__ volatile("syscall"
                     : "=a"(result)
                     : "a"(number), "D"(argument1), "S"(argument2), "d"(argument3)
                     : "rcx", "r11", "memory");
    return result;
}

__attribute__((noreturn)) void _start(void)
{
    const long sys_exit = 60;
    const long sys_getppid = 110;
    const long sys_sched_setaffinity = 203;
    unsigned long mask = 1UL << 1;
    long parent = syscall0(sys_getppid);
    long result = syscall3(sys_sched_setaffinity, parent, sizeof(mask), (long)&mask);

    syscall1(sys_exit, result < 0 ? 1 : 0);
    __builtin_unreachable();
}
