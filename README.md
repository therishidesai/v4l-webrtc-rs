# v4l-webrtc-rs

Live stream a v4l2 camera over webrtc to your browser

Use this [jsfiddle](https://jsfiddle.net/9s10amwL/) link to connect.

### Steps to run

- Copy Browser SDP from jsfiddle link
- `export BROWSER_SDP=<copied base64 SDP>`
- `echo $BROWSER_SDP | cargo run`
- copy the outputed base64 SDP into the text box on JSFiddle and click start session


NOTE: By Default we assume the camera is on `/dev/video4`
