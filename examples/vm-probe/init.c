/* PID 1 for the Nautilus guest handshake fixture. */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <linux/vm_sockets.h>
#include <sys/time.h>
#include <unistd.h>

#include "load_modules.h"

int main(void)
{
    if (load_modules() < 0)
        return 1;

    int server = socket(AF_VSOCK, SOCK_STREAM, 0);
    struct sockaddr_vm addr = {
        .svm_family = AF_VSOCK,
        .svm_cid = VMADDR_CID_ANY,
        .svm_port = 4081,
    };
    if (server < 0 || bind(server, (struct sockaddr *)&addr, sizeof(addr)) < 0 ||
        listen(server, 4) < 0) {
        perror("guest vsock");
        return 1;
    }

    for (;;) {
        int peer = accept(server, NULL, NULL);
        if (peer < 0) {
            if (errno == EINTR)
                continue;
            perror("accept");
            return 1;
        }
        const struct timeval timeout = { .tv_sec = 2 };
        char request[sizeof("PING 1\n") - 1];
        if (setsockopt(peer, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) == 0 &&
            recv(peer, request, sizeof(request), MSG_WAITALL) == sizeof(request) &&
            memcmp(request, "PING 1\n", sizeof(request)) == 0)
            send(peer, "PONG 1\n", 7, MSG_NOSIGNAL);
        close(peer);
    }
}
