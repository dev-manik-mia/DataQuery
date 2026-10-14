# SSH bastion for tunnel integration tests: password and key auth, TCP forwarding on.
FROM alpine:3.20
RUN apk add --no-cache openssh \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh dataquery \
 && echo 'dataquery:dataquery' | chpasswd \
 && sed -i -e 's/^#\?AllowTcpForwarding.*/AllowTcpForwarding yes/' \
           -e 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
# Test-only public keys for the .pem key tests (crates/dq-ssh/src/testdata).
COPY ssh-authorized_keys /home/dataquery/.ssh/authorized_keys
RUN chown -R dataquery:dataquery /home/dataquery/.ssh && chmod 700 /home/dataquery/.ssh \
 && chmod 600 /home/dataquery/.ssh/authorized_keys && passwd -u dataquery 2>/dev/null || true
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
