# SSH bastion for tunnel integration tests: password and key auth, TCP forwarding on.
FROM alpine:3.20
RUN apk add --no-cache openssh \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh dataquery \
 && echo 'dataquery:dataquery' | chpasswd \
 && sed -i -e 's/^#\?AllowTcpForwarding.*/AllowTcpForwarding yes/' \
           -e 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
