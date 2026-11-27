# Apache Cassandra 6.0-alpha2 packaged for deterministic multi-node Accord
# cluster tests (`examples/cluster/cassandra_accord.py`).
#
# Provides `cassandra-node`, a direct JVM launcher (avoiding shell/procps
# probes during VM boot) tuned for low-memory single-vCPU KVM guests.
{ pkgs }:
let
  version = "6.0-alpha2";
  src = pkgs.fetchurl {
    url = "https://archive.apache.org/dist/cassandra/${version}/apache-cassandra-${version}-bin.tar.gz";
    sha256 = "cc83cf03aee056646f76bd9d346e616fd3c2543f4a74e84b1431290b934c02d1";
  };
  jdk = pkgs.jdk17_headless;
in
pkgs.stdenv.mkDerivation {
  pname = "apache-cassandra";
  inherit version src;

  dontConfigure = true;
  dontBuild = true;

  installPhase = ''
    runHook preInstall
    mkdir -p "$out/share/cassandra" "$out/bin"
    cp -r bin conf lib "$out/share/cassandra/"

    cat > "$out/bin/cassandra-node" <<EOF
    #!${pkgs.bash}/bin/bash
    set -euo pipefail
    CONF_DIR="\''${1:-/build/conf}"
    DATA_DIR="\''${2:-/build/data}"
    CASSANDRA_HOME="$out/share/cassandra"
    export CASSANDRA_HOME
    export CASSANDRA_CONF="\$CONF_DIR"
    export CASSANDRA_LOG_DIR="\$DATA_DIR/logs"
    export JAVA_HOME="${jdk}"
    export PATH="${jdk}/bin:${pkgs.coreutils}/bin:${pkgs.bash}/bin:\$PATH"

    mkdir -p "\$DATA_DIR/data" "\$DATA_DIR/commitlog" "\$DATA_DIR/saved_caches" \
             "\$DATA_DIR/hints" "\$DATA_DIR/metadata" "\$DATA_DIR/accord_journal" \
             "\$DATA_DIR/logs"

    exec "${jdk}/bin/java" \\
      -Xms192m -Xmx320m -Xss256k \\
      -XX:+UseG1GC \\
      -XX:G1HeapRegionSize=4m \\
      -XX:ActiveProcessorCount=1 \\
      -XX:CICompilerCount=2 \\
      -XX:-UsePerfData \\
      -XX:+PerfDisableSharedMem \\
      -XX:+HeapDumpOnOutOfMemoryError \\
      -Djava.net.preferIPv4Stack=true \\
      -Dchronicle.analytics.disable=true \\
      -Dnet.bytebuddy.experimental \\
      -Djdk.attach.allowAttachSelf=true \\
      -Dio.netty.tryReflectionSetAccessible=true \\
      -Dio.netty.allocator.useCacheForAllThreads=true \\
      -Dio.netty.allocator.maxOrder=11 \\
      -Dcassandra-foreground=yes \\
      -Dcassandra.config="file://\$CONF_DIR/cassandra.yaml" \\
      -Dlogback.configurationFile="\$CONF_DIR/logback.xml" \\
      -Dcassandra.logdir="\$DATA_DIR/logs" \\
      -Dcassandra.storagedir="\$DATA_DIR" \\
      -Dcassandra.skip_wait_for_gossip_to_settle=0 \\
      -Dcassandra.ring_delay_ms=1000 \\
      -Dcassandra.consistent.rangemovement=false \\
      -Dcassandra.load_ring_state=false \\
      -Dcassandra.jmx.local.port=7199 \\
      --add-exports java.base/jdk.internal.misc=ALL-UNNAMED \\
      --add-exports java.management.rmi/com.sun.jmx.remote.internal.rmi=ALL-UNNAMED \\
      --add-exports java.management/com.sun.jmx.remote.security=ALL-UNNAMED \\
      --add-exports java.rmi/sun.rmi.registry=ALL-UNNAMED \\
      --add-exports java.rmi/sun.rmi.server=ALL-UNNAMED \\
      --add-exports java.sql/java.sql=ALL-UNNAMED \\
      --add-exports java.base/java.lang.ref=ALL-UNNAMED \\
      --add-exports jdk.unsupported/sun.misc=ALL-UNNAMED \\
      --add-opens java.base/java.lang.module=ALL-UNNAMED \\
      --add-opens java.base/jdk.internal.loader=ALL-UNNAMED \\
      --add-opens java.base/jdk.internal.ref=ALL-UNNAMED \\
      --add-opens java.base/jdk.internal.reflect=ALL-UNNAMED \\
      --add-opens java.base/jdk.internal.math=ALL-UNNAMED \\
      --add-opens java.base/jdk.internal.module=ALL-UNNAMED \\
      --add-opens java.base/jdk.internal.util.jar=ALL-UNNAMED \\
      --add-opens jdk.management/com.sun.management.internal=ALL-UNNAMED \\
      --add-opens java.base/sun.nio.ch=ALL-UNNAMED \\
      --add-opens java.base/java.io=ALL-UNNAMED \\
      --add-opens java.base/java.lang.reflect=ALL-UNNAMED \\
      --add-opens java.base/java.lang=ALL-UNNAMED \\
      --add-opens java.base/java.util=ALL-UNNAMED \\
      --add-opens java.base/java.nio=ALL-UNNAMED \\
      -javaagent:"\$CASSANDRA_HOME/lib/jamm-0.4.0.jar" \\
      -cp "\$CONF_DIR:\$CASSANDRA_HOME/lib/*" \\
      org.apache.cassandra.service.CassandraDaemon
    EOF
    chmod +x "$out/bin/cassandra-node"
    runHook postInstall
  '';

  meta.description = "Apache Cassandra ${version} with CEP-15 Accord transactions";
}
