FROM ghcr.io/vamsiramakrishnan/branchyard-agy@sha256:46e177cc5e874614a04cbe2b06b6df374ab50296557f27ef77246f609dbc158d
USER root
RUN install -d -m 0755 -o 1000 -g 1000 /workspace /branchyard/home \
    && printf 'branchyard-container-qualification-2026-11-02\n' > /workspace/.branchyard-image-contract \
    && chown 1000:1000 /workspace/.branchyard-image-contract
LABEL io.branchyard.qualification="gce-container-2026-11-02"
ENV HOME=/branchyard/home
WORKDIR /workspace
USER 1000:1000
