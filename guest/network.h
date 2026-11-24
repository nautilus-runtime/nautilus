/* Apply the bounded IPv4 configuration emitted by network::vm before READY.
 * Only PID 1 calls this. Older probe/lifecycle images have no network token. */
#include <spawn.h>
#include <sys/wait.h>
extern char **environ;

static bool guest_ip(char *const argv[])
{
    pid_t pid;
    if (posix_spawn(&pid, "/bin/busybox", NULL, NULL, argv, environ) != 0)
        return false;
    int status;
    while (waitpid(pid, &status, 0) < 0) {
        if (errno != EINTR)
            return false;
    }
    return WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

static bool write_resolver(const char *hex)
{
    if (mkdir("/etc", 0755) < 0 && errno != EEXIST)
        return false;
    FILE *out = fopen("/etc/resolv.conf", "w");
    if (!out)
        return false;
    bool ok = true;
    const char digits[] = "0123456789abcdef";
    while (*hex) {
        char *hi = strchr(digits, *hex++);
        if (!hi || !*hex) {
            ok = false;
            break;
        }
        char *lo = strchr(digits, *hex++);
        if (!lo || fputc((int)((hi - digits) * 16 + (lo - digits)), out) == EOF) {
            ok = false;
            break;
        }
    }
    return fclose(out) == 0 && ok;
}

/* Report what this boot applied. The host compares the whole token, including
 * routes and resolver, before it can publish this sandbox as ready. */
static char applied_network[1501];

static bool configure_pod_network(void)
{
    char cmdline[4096];
    if (!read_line("/proc/cmdline", cmdline, sizeof(cmdline)))
        return false;
    char *cursor = cmdline, *word, *config = NULL;
    const char key[] = "nautilus.net=";
    while ((word = strsep(&cursor, " \t"))) {
        if (!strncmp(word, key, sizeof(key) - 1)) {
            if (config)
                return false;
            config = word + sizeof(key) - 1;
        }
    }
    if (!config)
        return true;

    if (strlen(config) >= sizeof(applied_network))
        return false;
    strcpy(applied_network, config);
    char *addr = strsep(&config, "|"), *mtu = strsep(&config, "|");
    char *routes = strsep(&config, "|");
    if (!addr || !*addr || !mtu || !*mtu || !routes || !config || strchr(config, '|'))
        return false;
    if (!guest_ip((char *[]){"ip", "link", "set", "lo", "up", NULL}) ||
        !guest_ip((char *[]){"ip", "link", "set", "eth0", "mtu", mtu, "up", NULL}) ||
        !guest_ip((char *[]){"ip", "addr", "add", addr, "dev", "eth0", NULL}))
        return false;
    while (routes && *routes) {
        char *route = strsep(&routes, ",");
        char *dst = strsep(&route, "@");
        bool ok = route
            ? guest_ip((char *[]){"ip", "route", "add", dst, "via", route, "dev", "eth0", NULL})
            : guest_ip((char *[]){"ip", "route", "add", dst, "dev", "eth0", NULL});
        if (!ok)
            return false;
    }
    return write_resolver(config);
}
