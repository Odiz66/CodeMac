#!/usr/bin/env fish
# One-time setup so fisch-macro can create its virtual mouse + keyboard
# through /dev/uinput without running as root.
# Usage: fish scripts/setup-uinput.fish   (asks for your sudo password)

set -l rule_file /etc/udev/rules.d/99-fisch-macro-uinput.rules
set -l rule 'KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"'

echo "Writing $rule_file"
echo $rule | sudo tee $rule_file >/dev/null; or exit 1

echo "Loading the uinput module now and on every boot"
echo uinput | sudo tee /etc/modules-load.d/uinput.conf >/dev/null; or exit 1
sudo modprobe uinput; or exit 1

echo "Adding $USER to the input group"
sudo usermod -aG input $USER; or exit 1

echo "Applying the rule"
sudo udevadm control --reload-rules
sudo udevadm trigger --name-match=uinput
# Make the current boot right even if the trigger did not touch the node.
sudo chgrp input /dev/uinput
sudo chmod 0660 /dev/uinput

ls -l /dev/uinput
echo
echo "Done. Log out and back in (or reboot) so the new group membership applies,"
echo "then check with:  id -nG | string match -q '*input*'; and echo ok"
