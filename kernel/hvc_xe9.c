// SPDX-License-Identifier: GPL-2.0
/*
 * microvm: bidirectional "portb" hypervisor console over I/O ports.
 *
 * Replaces the 16550 UART as the guest console. It registers with the hvc_console
 * framework as hvc0, which gives a full interactive tty (input included) while keeping the
 * cheap one-outb-per-byte output path of the earlycon=xe9 console.
 *
 * Wire protocol with the microvm VMM:
 *   - outb(byte, 0xE9)  : transmit one byte to the host console (one VM exit per byte);
 *   - inb(0xEA) & 1     : receive status, set when a host->guest byte is available;
 *   - inb(0xE9)         : read (consume) the next received byte.
 *
 * There is no interrupt line: hvc polls via its own timer thread, which is fine for a
 * console. Select it with "console=hvc0" (add_preferred_console also makes it the default).
 */
#include <linux/console.h>
#include <linux/err.h>
#include <linux/init.h>
#include <linux/types.h>

#include <asm/io.h>

#include "hvc_console.h"

#define XE9_DATA	0xe9	/* out: TX byte;  in: RX byte	*/
#define XE9_STATUS	0xea	/* in: bit0 = RX data available	*/

static ssize_t hvc_xe9_put(uint32_t vtermno, const u8 *buf, size_t count)
{
	size_t i;

	for (i = 0; i < count; i++)
		outb(buf[i], XE9_DATA);

	return count;
}

static ssize_t hvc_xe9_get(uint32_t vtermno, u8 *buf, size_t count)
{
	size_t i;

	for (i = 0; i < count; i++) {
		if (!(inb(XE9_STATUS) & 1))
			break;
		buf[i] = inb(XE9_DATA);
	}

	return i;
}

static const struct hv_ops hvc_xe9_ops = {
	.get_chars = hvc_xe9_get,
	.put_chars = hvc_xe9_put,
};

static int __init hvc_xe9_init(void)
{
	struct hvc_struct *hp;

	hp = hvc_alloc(0, 0, &hvc_xe9_ops, 128);
	if (IS_ERR(hp))
		return PTR_ERR(hp);

	return 0;
}
device_initcall(hvc_xe9_init);

static int __init hvc_xe9_console_init(void)
{
	hvc_instantiate(0, 0, &hvc_xe9_ops);
	add_preferred_console("hvc", 0, NULL);

	return 0;
}
console_initcall(hvc_xe9_console_init);
