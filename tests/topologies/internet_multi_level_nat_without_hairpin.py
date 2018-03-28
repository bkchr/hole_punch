"""
Internet like topology with multi level nat that does not support hairpining.

        server
           |
           s0
           |
          nat1
           |
           s1
           |
    ----------------
    |              |
   nat2           nat3
    |              |
   s2              s3
    |              |
  client          peer
"""

from mininet.topo import Topo
from mininet.nodelib import NAT


class Topology(Topo):
    def __init__(self, **opts):
        Topo.__init__(self, **opts)

        # set up server switch
        server_switch = self.addSwitch('s0')
        # add server
        server = self.addHost('server')
        self.addLink(server_switch, server)

        # create nat1 and s1
        s1 = self.add_nat_and_switch(server_switch, 1)

        s2 = self.add_nat_and_switch(
            s1, 2, ip="192.168.1.100", default_route="via 192.168.1.1")

        s3 = self.add_nat_and_switch(
            s1, 3, ip="192.168.1.101", default_route="via 192.168.1.1")

        self.add_host("client", s2, 2)
        self.add_host("peer", s3, 3)

    def add_host(self, name, switch, subnet):
        local_ip = '192.168.%d.1' % subnet
        # add host and connect to local switch
        host = self.addHost(
            name,
            ip='192.168.%d.100/24' % subnet,
            defaultRoute='via %s' % local_ip)
        self.addLink(host, switch)

    def add_nat_and_switch(self,
                           inet_switch,
                           subnet,
                           ip=None,
                           default_route=None):
        inet_if = 'nat%d-eth0' % subnet
        local_if = 'nat%d-eth1' % subnet
        local_ip = '192.168.%d.1' % subnet
        local_subnet = '192.168.%d.0/24' % subnet
        nat_params = {'ip': '%s/24' % local_ip}

        nat_node_args = {
            "cls": NAT,
            "subnet": local_subnet,
            "inetIntf": inet_if,
            "localIntf": local_if
        }

        if ip is not None:
            nat_node_args["ip"] = ip

        if default_route is not None:
            nat_node_args["defaultRoute"] = default_route

        # add NAT to topology
        nat = self.addNode('nat%d' % subnet, **nat_node_args)
        switch = self.addSwitch('s%d' % subnet)

        # connect NAT to inet and local switches
        self.addLink(nat, inet_switch, intfName1=inet_if)
        self.addLink(nat, switch, intfName1=local_if, params1=nat_params)

        return switch
