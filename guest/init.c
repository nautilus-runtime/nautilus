/* Linux PID 1 for a Nautilus Pod VM. */
#include <stdbool.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <linux/vm_sockets.h>

#include "load_modules.h"

enum { UUID_LEN = 36, IDENTITY_PORT = 4082 };

static bool canonical_uuid(const char *value)
{
    if (strlen(value) != UUID_LEN)
        return false;
    bool nonzero = false;
    for (size_t i = 0; i < UUID_LEN; ++i) {
        char c = value[i];
        if (i == 8 || i == 13 || i == 18 || i == 23) {
            if (c != '-')
                return false;
        } else {
            if (!(c >= '0' && c <= '9') && !(c >= 'a' && c <= 'f'))
                return false;
            nonzero |= c != '0';
        }
    }
    return nonzero;
}

static bool mount_guest_fs(const char *type, const char *path, unsigned long flags)
{
    if (mkdir(path, 0755) < 0 && errno != EEXIST)
        return false;
    return mount(type, path, type, flags, NULL) == 0 || errno == EBUSY;
}

/* procfs vends newline-terminated text. Reject truncation before tokenizing. */
static bool read_line(const char *path, char *buffer, size_t capacity)
{
    FILE *file = fopen(path, "r");
    if (!file)
        return false;
    bool ok = fgets(buffer, capacity, file) != NULL;
    fclose(file);
    if (ok) {
        char *newline = strchr(buffer, '\n');
        if (newline) {
            *newline = '\0';
            return true;
        }
    }
    return false;
}

#include "network.h"

static bool make_identity(char *response, size_t size)
{
    char cmdline[4096], boot[UUID_LEN + 2];
    if (!read_line("/proc/cmdline", cmdline, sizeof(cmdline)) ||
        !read_line("/proc/sys/kernel/random/boot_id", boot, sizeof(boot)) ||
        !canonical_uuid(boot))
        return false;
    const char *sandbox = NULL, *nonce = NULL;
    char *cursor = cmdline, *arg;
    while ((arg = strsep(&cursor, " \t"))) {
        const char sandbox_key[] = "nautilus.sandbox_id=", nonce_key[] = "nautilus.nonce=";
        if (strncmp(arg, sandbox_key, sizeof(sandbox_key) - 1) == 0) {
            if (sandbox)
                return false;
            sandbox = arg + sizeof(sandbox_key) - 1;
        } else if (strncmp(arg, nonce_key, sizeof(nonce_key) - 1) == 0) {
            if (nonce)
                return false;
            nonce = arg + sizeof(nonce_key) - 1;
        }
    }
    if (!sandbox || !nonce || !canonical_uuid(sandbox) || !canonical_uuid(nonce) ||
        !configure_pod_network())
        return false;
    int length = snprintf(response, size, "IDENTITY 1 %s %s %s\n", sandbox, nonce, boot);
    return length > 0 && (size_t)length < size;
}

static void identify(int peer, const char *response)
{
    const struct timeval timeout = { .tv_sec = 1 };
    if (setsockopt(peer, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) < 0 ||
        setsockopt(peer, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) < 0)
        return;
    const char query[] = "IDENTIFY 1\n";
    char request[sizeof(query) - 1];
    if (recv(peer, request, sizeof(request), MSG_WAITALL) != (ssize_t)sizeof(request))
        return;
    char network[1700];
    if (memcmp(request, "NETWORK? 1\n", sizeof(request)) == 0 && applied_network[0]) {
        /* Identity ends with the canonical boot UUID and a newline. */
        const char *boot = strrchr(response, ' ');
        if (!boot || strlen(boot + 1) != UUID_LEN + 1)
            return;
        int n = snprintf(network, sizeof(network), "NETWORK 1 %.*s nautilus.net=%s\n",
                         UUID_LEN, boot + 1, applied_network);
        if (n <= 0 || (size_t)n >= sizeof(network))
            return;
        response = network;
    } else if (memcmp(request, query, sizeof(request)) != 0) {
        return;
    }
    size_t left = strlen(response);
    while (left > 0) {
        ssize_t n = send(peer, response, left, MSG_NOSIGNAL);
        if (n <= 0)
            return;
        response += n;
        left -= (size_t)n;
    }
}

int main(void)
{
    if (!mount_guest_fs("proc", "/proc", MS_NOSUID | MS_NOEXEC | MS_NODEV) ||
        load_modules() < 0) {
        perror("guest initialization");
        return EXIT_FAILURE;
    }
    char response[128];
    if (!make_identity(response, sizeof(response))) {
        fputs("invalid guest boot identity\n", stderr);
        return EXIT_FAILURE;
    }
    /* The identity-only image can boot without a filesystem export. */
    if (access("/bin/nautilus-agent", X_OK) == 0) {
        char sandbox[37], nonce[37], boot[37];
        if (sscanf(response, "IDENTITY 1 %36s %36s %36s", sandbox, nonce, boot) != 3)
            return EXIT_FAILURE;
        /* Reap orphaned workload descendants after the agent has exited. */
        signal(SIGCHLD, SIG_IGN);
        pid_t child = fork();
        if (child == 0) {
            signal(SIGCHLD, SIG_DFL);
            execl("/bin/nautilus-agent", "nautilus-agent", sandbox, nonce, boot, (char *)NULL);
            perror("exec nautilus-agent");
            _exit(1);
        }
        if (child < 0) {
            perror("fork nautilus-agent");
            return EXIT_FAILURE;
        }
    }
    int server = socket(AF_VSOCK, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_vm addr = {
        .svm_family = AF_VSOCK, .svm_cid = VMADDR_CID_ANY, .svm_port = IDENTITY_PORT,
    };
    if (server < 0 || bind(server, (struct sockaddr *)&addr, sizeof(addr)) < 0 ||
        listen(server, 8) < 0) {
        perror("guest vsock");
        return EXIT_FAILURE;
    }
    for (;;) {
        struct sockaddr_vm sender = {0};
        socklen_t len = sizeof(sender);
        int peer = accept4(server, (struct sockaddr *)&sender, &len, SOCK_CLOEXEC);
        if (peer < 0) {
            if (errno == EINTR)
                continue;
            perror("accept");
            return EXIT_FAILURE;
        }
        if (len == sizeof(sender) && sender.svm_family == AF_VSOCK && sender.svm_cid == VMADDR_CID_HOST)
            identify(peer, response);
        close(peer);
    }
}
