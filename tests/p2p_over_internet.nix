import <nixpkgs/nixos/tests/make-test.nix> ({ pkgs, lib, ... }:
let
  server_bin = pkgs.stdenv.mkDerivation {
    name = "server";
    phases = [ "buildPhase" ];
    server = ./server.sh;
    buildPhase = "mkdir -p $out/bin && cp $server $out/bin/hole_punch_server";
  };
in
  {
    name = "P2POverInternet";

    nodes =
      { client =
          { config, pkgs, nodes, ... }: {
            virtualisation.vlans = [ 1 ];
            networking.firewall.allowPing = true;
            networking.defaultGateway =
              (pkgs.lib.head nodes.client_router.config.networking.interfaces.eth2.ip4).address;
          };

        peer =
          { config, pkgs, nodes, ... }: {
            virtualisation.vlans = [ 3 ];
            networking.firewall.allowPing = true;
            networking.defaultGateway =
              (pkgs.lib.head nodes.peer_router.config.networking.interfaces.eth2.ip4).address;
          };

        client_router =
          { config, pkgs, ... }: {
            virtualisation.vlans = [ 2 1 ];
            networking.firewall.enable = true;
            networking.firewall.allowPing = true;
            networking.nat.internalIPs = [ "192.168.1.0/24" ];
            networking.nat.externalInterface = "eth1";
            networking.nat.enable = true;
          };

        peer_router =
          { config, pkgs, ... }: {
            virtualisation.vlans = [ 2 3 ];
            networking.firewall.enable = true;
            networking.firewall.allowPing = true;
            networking.nat.internalIPs = [ "192.168.3.0/24" ];
            networking.nat.externalInterface = "eth1";
            networking.nat.enable = true;
          };

        server =
          { config, pkgs, ... }: {
            virtualisation.vlans = [ 2 ];
            networking.firewall.enable = false;
            environment.systemPackages = [ server_bin ];
          };
      };

    testScript =
      ''
        #startAll;
        $server->start;
        #$peer_router->start;
        #$client_router->start;

        # The router should have access to the server.
        $server->waitForUnit("network.target");
        $server->succeed("hole_punch_server");

        # Make sure that the client router reaches the server
        $client_router->waitForUnit("network.target");
        $client_router->succeed("ping -c 1 server >&2");

        # Same for the peer router
        $peer_router->waitForUnit("network.target");
        $peer_router->succeed("ping -c 1 server >&2");

        # Make sure that the firewalls are running
        $client_router->waitForUnit("firewall");
        $peer_router->waitForUnit("firewall");

        # The routers need to reach each other
        $peer_router->succeed("ping -c 1 client_router >&2");
        $client_router->succeed("ping -c 1 peer_router >&2");

        $peer->start;
        $client->start;

        # The client needs to reach the server
        $client->waitForUnit("network.target");
        $client->succeed("ping -c 1 server >&2");

        # The peer needs to reach the server
        $peer->waitForUnit("network.target");
        $peer->succeed("ping -c 1 server >&2");

        # The client should not be able to ping the peer
        $client->fail("ping -c 1 peer >&2");
      '';
  })

