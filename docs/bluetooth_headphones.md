# Bluetooth headphone volume during dictation on macOS

When an app opens a Bluetooth headset's microphone, macOS can switch the headset from stereo playback to a mode that supports both recording and playback. Apple documents a change in sound quality and volume during this switch. A volume increase alone does not establish that the switch caused it. [Apple's explanation of Bluetooth audio modes](https://support.apple.com/en-ie/102217)

To isolate the trigger, keep the headset connected as the Mac's output and change only **General → Sound → Microphone** in Handy. Compare the headset microphone with a USB mic or the Mac's built-in mic with the lid open. Use a low playback level. If switching the input resolves the problem, you can keep that microphone selected for dictation and continue listening through the headphones.

If you need the headset microphone and music gets louder, watch the macOS output volume in Control Center. If it rises when recording starts and returns to its old position after recording stops, try lowering it **while the headset microphone is active**. To test this, take the headphones off, pause playback, and temporarily turn off **Mute While Recording**. Start a recording, lower the output slider, then stop and start another recording to see whether the level is remembered. Restore your preferred mute setting afterwards.

If only Handy's start and stop tones are loud, turn off **Audio Feedback** for one recording. If your own voice sounds amplified, some Bose models have a **Self Voice** setting in the Bose app that controls how much of your voice you hear during calls. [Bose QuietComfort Ultra user guide](https://assets.bose.com/content/dam/Bose_DAM/Web/consumer_electronics/global/products/headphones/qcuh2-headphonearn/pdfs/897194_OG_QCUH2-HEADPHONEARN_ML.pdf)

With on-demand recording, **Mute While Recording** leaves the start and stop tones audible. Handy opens the microphone, plays the start tone if enabled, then mutes system output. On stop it restores the previous mute state before playing the stop tone and closing the microphone. These intervals can expose a playback level change at either end of a muted recording.
