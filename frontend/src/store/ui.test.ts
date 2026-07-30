import { beforeEach, describe, expect, it } from 'vitest';

import { releaseImagePin, useUiStore } from './ui';

/*
 * SDD §5.9: "the app switches to `STACK` when a capture sequence starts, and the operator can
 * switch back at will."
 *
 * Both halves are the requirement, and the second is the one a naive implementation loses: an app
 * that switches on every frame drags the operator back to STACK sixty times during a sixty-frame
 * sequence, so "at will" lasts until the next exposure. These tests are about the pin that makes
 * the override stick.
 */

const initial = useUiStore.getState();

beforeEach(() => {
  useUiStore.setState({
    destination: initial.destination,
    imageSource: initial.imageSource,
    imagePinned: false,
    systemOpen: false,
  });
});

describe('the shared image surface', () => {
  it('starts on FRAME, because framing is what an operator does before there is a stack', () => {
    expect(useUiStore.getState().imageSource).toBe('frame');
    expect(useUiStore.getState().imagePinned).toBe(false);
  });

  it('switches to STACK when a capture starts', () => {
    useUiStore.getState().captureStarted();
    expect(useUiStore.getState().imageSource).toBe('stack');
  });

  it('leaves the operator on FRAME once they have chosen it, for the whole sequence', () => {
    useUiStore.getState().captureStarted();
    expect(useUiStore.getState().imageSource).toBe('stack');

    // The operator switches back to check focus mid-sequence.
    useUiStore.getState().showImage('frame');
    expect(useUiStore.getState().imageSource).toBe('frame');

    // Every subsequent frame of the same sequence reports a capture. None of them may move it.
    for (let frame = 0; frame < 10; frame += 1) {
      useUiStore.getState().captureStarted();
    }
    expect(useUiStore.getState().imageSource).toBe('frame');
  });

  it('may be moved again by the next sequence, once the pin is released', () => {
    useUiStore.getState().showImage('frame');
    useUiStore.getState().captureStarted();
    expect(useUiStore.getState().imageSource).toBe('frame');

    // The sequence ends.
    releaseImagePin();
    useUiStore.getState().captureStarted();
    expect(useUiStore.getState().imageSource).toBe('stack');
  });

  it('follows the phone navigation, because there the bottom bar is the source toggle', () => {
    useUiStore.getState().show('stack');
    expect(useUiStore.getState().destination).toBe('stack');
    expect(useUiStore.getState().imageSource).toBe('stack');

    useUiStore.getState().show('frame');
    expect(useUiStore.getState().imageSource).toBe('frame');
  });

  it('does not move the picture when the operator goes to TARGET, which is not a source', () => {
    useUiStore.getState().showImage('stack');
    useUiStore.getState().show('target');

    expect(useUiStore.getState().destination).toBe('target');
    expect(useUiStore.getState().imageSource).toBe('stack');
  });

  it('treats a phone navigation choice as an override, like the tablet toggle', () => {
    useUiStore.getState().show('frame');
    useUiStore.getState().captureStarted();
    expect(useUiStore.getState().imageSource).toBe('frame');
  });

  it('leaves the system detour on any destination choice', () => {
    useUiStore.getState().openSystem();
    expect(useUiStore.getState().systemOpen).toBe(true);
    useUiStore.getState().show('stack');
    expect(useUiStore.getState().systemOpen).toBe(false);
  });
});
