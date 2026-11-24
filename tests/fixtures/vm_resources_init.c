/* Test PID 1. Respond to the normal startup handshake only if Linux actually
 * brought the requested CPUs and memory online. */
#define main identity_main
#include "../../guest/init.c"
#undef main
#include <sys/sysinfo.h>

int main(void)
{
    struct sysinfo info;
    char cpus[32];
    if (!mount_guest_fs("sysfs", "/sys", MS_NOSUID | MS_NOEXEC | MS_NODEV) ||
        !read_line("/sys/devices/system/cpu/online", cpus, sizeof(cpus)) ||
        strcmp(cpus, "0-1") != 0 || sysinfo(&info) < 0)
        return EXIT_FAILURE;
    /* Some assigned RAM is reserved for the kernel, initramfs and firmware. */
    unsigned long long bytes = (unsigned long long)info.totalram * info.mem_unit;
    if (bytes < 384ULL << 20 || bytes > 448ULL << 20)
        return EXIT_FAILURE;
    return identity_main();
}
