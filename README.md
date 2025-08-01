# description
Download docker image tar file from hub.docker.com and ghcr.io. 

# usage
1. Download binary file.
2. Open terminal.
3. (*optional*) set proxy env. HTTP_PROXY and HTTPS_PROXY. In powershell, it should be `$env:HTTP_PROXY="http://127.0.0.1:7890"; $env:HTTPS_PROXY="http://127.0.0.1:7890"`.
4. Run `./DockerImageTarDownloadTool [registry/][repository/]image[:tag|@digest]`.
5. image.tar will at the terminal current workspace dir.
# note
1. By default, tool use dockerhub registry, so simply usage is `./DockerImageTarDownloadTool nginx:latest`.
2. Each layer download timeout is `300s`


# build from source
## 1. build binary in local
`cargo build --release`

## 2. use cross build binary 
### 2.1 install cross
`cargo install cross`
for more info https://github.com/cross-rs/cross
### 2.2 build
`cross build --target x86_64-pc-windows-gnu --release`
